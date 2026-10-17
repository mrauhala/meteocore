//! The snapshot a poll publishes, and the one selection of a run and step
//! that the render and both cache-key resolvers go through.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::geo::Crs;
use ds_core::map_engine::{ParameterInfo, RasterInfo};
use ds_core::resample::SourceGrid;
use ds_core::vertical::{VerticalDimension, VerticalKind};
use ds_core::wind::{GridAxes, ParameterFacts, VectorFrame, WindFacts};
use ds_storage::ObjectVersion;
use netcdf_reader::NcFile;

use crate::ingest::{FieldRead, FileGrid, Variable};
use crate::remote::RemoteFile;

/// Which version of a file the catalog read: its size and modification
/// time (a local file's mtime, a remote one's `Last-Modified`), and a
/// remote ETag where the server sends one. A file whose version changes is
/// another file, ingested again (#1067 decision 7).
pub(crate) type Identity = ObjectVersion;

/// The identity of a local file from its metadata.
pub(crate) fn local_identity(metadata: &std::fs::Metadata) -> Identity {
    ObjectVersion {
        size: metadata.len(),
        last_modified: metadata.modified().ok().map(DateTime::<Utc>::from),
        e_tag: None,
    }
}

/// A catalogued file, open for as long as the catalog keeps it. Requests
/// read through its handle and never open a file.
pub(crate) struct OpenFile {
    /// Unique in the process: the field cache's key ([`crate::field`]).
    pub id: u64,
    /// The local path, or the object key of a remote file.
    pub path: PathBuf,
    pub identity: Identity,
    pub nc: NcFile,
    /// The storage of a remote file, read by HTTP range.
    pub remote: Option<Arc<RemoteFile>>,
    pub run: Option<DateTime<Utc>>,
    /// Valid time of each index along the time axis.
    pub times: Vec<DateTime<Utc>>,
    /// How to read each of [`Layout::variables`], in its order.
    pub reads: Vec<FieldRead>,
}

/// The collection's variables and grid, fixed by the first file ingested:
/// every later file must carry the same variables on the same grid.
pub(crate) struct Layout {
    pub variables: Vec<Variable>,
    pub grid: FileGrid,
    /// The sampler over the grid, built once.
    pub source: SourceGrid,
    /// What the files state about wind components, for
    /// `ds_core::wind::DerivedWind`: built once with the layout, so the
    /// snapshot is the same `Arc` for the engine's life (#897).
    pub wind: Arc<WindFacts>,
    /// The collection's vertical axis (#1067 PR 8): its variables' pressure
    /// levels in hPa, bottom first ([`Variable::pressure`]), as
    /// `RasterInfo::vertical` and EDR's vertical extent advertise it. `None`
    /// when the variables have one level each.
    pub vertical: Option<VerticalDimension>,
}

impl Layout {
    pub(crate) fn new(variables: Vec<Variable>, grid: FileGrid) -> Self {
        let wind = Arc::new(wind_facts(&variables, &grid.transform.crs));
        // Every variable of a layout is on the same levels, or none is
        // (`ingest::one_vertical_axis`).
        let vertical = variables
            .first()
            .and_then(|v| v.pressure.clone())
            .map(|levels| VerticalDimension::new(VerticalKind::Pressure, levels));
        Self {
            source: SourceGrid::new(grid.transform.clone()),
            variables,
            grid,
            wind,
            vertical,
        }
    }

    /// The level a map draws, as an index into the vertical axis's levels:
    /// `z` (WMS `ELEVATION`, Maps/Tiles `elevation`), which must be one of
    /// them exactly, else the first, the bottom level. 0 without a vertical
    /// axis, where the API layers refuse a `z` before it gets here.
    ///
    /// The render's one level selection, so the default
    /// `MapEngine::resolve_elevation` (`z`, else the first advertised level)
    /// names the level drawn: GetFeatureInfo queries that level, and the
    /// rendered caches, which key on the requested `z`, hold one level per
    /// key.
    pub(crate) fn map_level(&self, z: Option<f64>) -> Result<usize, DataServerError> {
        let (Some(vertical), Some(z)) = (&self.vertical, z) else {
            return Ok(0);
        };
        vertical
            .levels
            .iter()
            .position(|&level| level == z)
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!(
                    "Level {z} is not one of this collection's pressure levels: {} hPa",
                    crate::ingest::list_levels(&vertical.levels)
                ))
            })
    }

    /// The levels an EDR query reads, as indices into the vertical axis's
    /// levels: every level without `z` (`/req/edr/z-response` F, as
    /// engine-grib's level views answer); else those `z` names, in request
    /// order and each once, a level the collection lacks dropped
    /// (clause B), and none of them a 400. `[0]` without a vertical axis,
    /// where api-edr has already ignored any `z` (clause A).
    pub(crate) fn query_levels(&self, z: Option<&[f64]>) -> Result<Vec<usize>, DataServerError> {
        let Some(vertical) = &self.vertical else {
            return Ok(vec![0]);
        };
        let Some(z) = z else {
            return Ok((0..vertical.levels.len()).collect());
        };
        let mut levels = Vec::with_capacity(z.len());
        for value in z {
            if let Some(i) = vertical.levels.iter().position(|level| level == value) {
                if !levels.contains(&i) {
                    levels.push(i);
                }
            }
        }
        if levels.is_empty() {
            return Err(DataServerError::InvalidParameter(format!(
                "The requested vertical levels select none of this collection's pressure \
                 levels: {} hPa",
                crate::ingest::list_levels(&vertical.levels)
            )));
        }
        Ok(levels)
    }
}

/// The wind facts of a layout (#897, #1067 decision 6): each variable's CF
/// `standard_name` says whether it is a component and in which frame
/// (`x_wind`/`y_wind`: along the grid's axes; `eastward_wind`/
/// `northward_wind`: along east and north), its single level is the
/// pairing level, and the grid's projection says how its axes lie. A
/// variable on the pressure axis has no pairing level (`level: None`, as
/// `ParameterFacts` asks of a parameter spanning the vertical axis):
/// `DerivedWind` reads u and v at the one `z` it is asked for, so
/// `x_wind_pl`/`y_wind_pl` pair level by level (#1067 PR 8). MEPS's
/// Lambert grid is `GridAxes::Rotated`, so its grid-relative 10 m wind
/// gives a speed and no direction until the components are turned by the
/// convergence angle (a follow-up on #897). Nothing is taken from a name.
pub(crate) fn wind_facts(variables: &[Variable], crs: &Crs) -> WindFacts {
    WindFacts {
        grid: GridAxes::of(crs),
        parameters: variables
            .iter()
            .map(|v| {
                let level = (!v.levels.is_empty()).then(|| {
                    v.levels
                        .iter()
                        .map(|l| format!("{} {}", l.value, l.units).trim().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                });
                ParameterFacts {
                    standard_name: v.standard_name.clone(),
                    frame: v
                        .standard_name
                        .as_deref()
                        .map_or(VectorFrame::Unknown, VectorFrame::from_standard_name),
                    level_label: level.clone(),
                    level,
                    unit: v.units.clone(),
                    ..ParameterFacts::new(v.name.to_string())
                }
            })
            .collect(),
    }
}

/// A run's key: its reference time, `None` for the timeline of files
/// without one.
pub(crate) type RunKey = Option<DateTime<Utc>>;

/// One field's place: a file and an index along its time axis.
#[derive(Clone)]
pub(crate) struct Step {
    pub file: Arc<OpenFile>,
    pub index: u32,
}

/// The valid times of one model run, or of the one timeline of files
/// without a run.
pub(crate) struct Run {
    pub reference_time: Option<DateTime<Utc>>,
    pub steps: BTreeMap<DateTime<Utc>, Step>,
}

impl Run {
    /// Whether `time` lies within the run's first and last step.
    fn covers(&self, time: DateTime<Utc>) -> bool {
        match (self.steps.first_key_value(), self.steps.last_key_value()) {
            (Some((&first, _)), Some((&last, _))) => first <= time && time <= last,
            _ => false,
        }
    }

    /// The step nearest `time` within the run, the earlier on a tie;
    /// `None` outside the run.
    fn nearest(&self, time: DateTime<Utc>) -> Option<(DateTime<Utc>, &Step)> {
        if !self.covers(time) {
            return None;
        }
        let before = self.steps.range(..=time).next_back();
        let after = self.steps.range(time..).next();
        before
            .into_iter()
            .chain(after)
            .min_by_key(|(&valid, _)| (valid - time).abs())
            .map(|(&valid, step)| (valid, step))
    }
}

/// A step [`Catalog::select`] chose.
pub(crate) struct Selected<'a> {
    pub reference_time: Option<DateTime<Utc>>,
    pub valid: DateTime<Utc>,
    pub step: &'a Step,
}

/// What the engine serves, swapped whole by each poll.
pub(crate) struct Catalog {
    pub layout: Option<Arc<Layout>>,
    /// By reference time, ascending. Files without one form the single
    /// timeline under `None`; a catalog never holds both kinds.
    pub runs: BTreeMap<RunKey, Run>,
    pub info: Arc<RasterInfo>,
    /// [`ds_core::map_engine::MapEngine::content_version`]: moves when a
    /// catalogued file is replaced on disk.
    pub content_version: u64,
    /// When the last poll listed the directory.
    pub polled_at: Option<DateTime<Utc>>,
}

impl Catalog {
    /// The catalog before the first scan: no layers, no data.
    pub(crate) fn empty(content_version: u64) -> Self {
        Self {
            layout: None,
            runs: BTreeMap::new(),
            info: Arc::new(raster_info(None, &BTreeMap::new(), &BTreeMap::new())),
            content_version,
            polled_at: None,
        }
    }

    /// The run and step a map of `time` from `reference_time` draws, as
    /// engine-grib's default layer selects them: a pinned run is exactly that
    /// run, absent ⇒ [`DataServerError::ReferenceTimeNotFound`] (404), and
    /// never falls back to another. Unpinned, the newest run whose steps
    /// span `time`, so an incomplete newer run does not hide an older one
    /// that covers it; without a time, the newest run. Within the run, the
    /// step nearest `time` (the earlier on a tie), or its last step without
    /// one. A time outside the chosen run is
    /// [`DataServerError::InvalidParameter`].
    ///
    /// The render, `resolve_time` and `resolve_reference_time` all call
    /// this, so the cache keys name exactly what is drawn (#507, #521):
    /// pinning the run it picks picks the same step again.
    pub(crate) fn select(
        &self,
        reference_time: Option<DateTime<Utc>>,
        time: Option<DateTime<Utc>>,
    ) -> Result<Selected<'_>, DataServerError> {
        if self.runs.is_empty() {
            return Err(DataServerError::Engine(
                "No NetCDF data available yet".to_string(),
            ));
        }
        let run = match (reference_time, time) {
            (Some(rt), _) => self.runs.get(&Some(rt)).ok_or_else(|| {
                DataServerError::ReferenceTimeNotFound(format!(
                    "no forecast run for reference time {rt}"
                ))
            })?,
            (None, Some(t)) => self
                .runs
                .values()
                .rev()
                .find(|run| run.covers(t))
                .ok_or_else(|| {
                    DataServerError::InvalidParameter(format!("No forecast run covers time {t}"))
                })?,
            (None, None) => self.runs.values().next_back().expect("runs is not empty"),
        };
        let (valid, step) = match time {
            Some(t) => run.nearest(t).ok_or_else(|| {
                DataServerError::InvalidParameter(format!("No forecast step for time {t}"))
            })?,
            None => run
                .steps
                .last_key_value()
                .map(|(&valid, step)| (valid, step))
                .ok_or_else(|| DataServerError::Engine("forecast run has no steps".into()))?,
        };
        Ok(Selected {
            reference_time: run.reference_time,
            valid,
            step,
        })
    }

    /// The run an EDR query reads, as engine-grib selects it
    /// (`/req/core/datetime-response` A and F): a pinned run is exactly
    /// that run (absent ⇒ [`DataServerError::ReferenceTimeNotFound`], 404).
    /// Unpinned with a `datetime` window, the newest run with a step inside
    /// it that also spans the window's start, else the newest with any step
    /// inside, which is how an open start (`../end`) or a window starting
    /// before a run finds one; none ⇒ [`no_step_within`] (404), so a
    /// `datetime` list skips the instant (#968). Without a window, the
    /// newest run. An instant `(t, t)` needs a step valid at exactly `t`,
    /// never the nearest: GetFeatureInfo asks for the step its map drew,
    /// in the run the map drew it from.
    pub(crate) fn query_run(
        &self,
        reference_time: Option<DateTime<Utc>>,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<&Run, DataServerError> {
        if self.runs.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No NetCDF data available yet".into(),
            ));
        }
        if let Some(rt) = reference_time {
            return self.runs.get(&Some(rt)).ok_or_else(|| {
                DataServerError::ReferenceTimeNotFound(format!(
                    "no forecast run for reference time {rt}"
                ))
            });
        }
        let Some((start, end)) = window else {
            return Ok(self.runs.values().next_back().expect("runs is not empty"));
        };
        let newest_first = || {
            self.runs
                .values()
                .rev()
                .filter(move |run| run.steps_within(start, end).next().is_some())
        };
        newest_first()
            .find(|run| run.covers(start))
            .or_else(|| newest_first().next())
            .ok_or_else(|| no_step_within(start, end))
    }
}

impl Run {
    /// The steps valid inside `[start, end]`, both ends included, ascending.
    pub(crate) fn steps_within(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> impl Iterator<Item = (DateTime<Utc>, &Step)> + '_ {
        // `range` panics on a reversed range, which the API refuses (#932).
        let start = start.min(end);
        self.steps
            .range(start..=end)
            .map(|(&valid, step)| (valid, step))
    }
}

/// No step is valid inside the requested `datetime`: the 404 an instant
/// without data gets, which a `datetime` list skips (#968).
pub(crate) fn no_step_within(start: DateTime<Utc>, end: DateTime<Utc>) -> DataServerError {
    let bound = |time: DateTime<Utc>| {
        if time == DateTime::<Utc>::MIN_UTC || time == DateTime::<Utc>::MAX_UTC {
            "..".to_string()
        } else {
            time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }
    };
    let datetime = if start == end {
        bound(start)
    } else {
        format!("{}/{}", bound(start), bound(end))
    };
    DataServerError::LocationNotFound(format!("No forecast step matches datetime {datetime}"))
}

/// The `RasterInfo` of a catalog, built once per poll (Critical Rule 10).
pub(crate) fn raster_info(
    layout: Option<&Layout>,
    runs: &BTreeMap<RunKey, Run>,
    titles: &BTreeMap<String, String>,
) -> RasterInfo {
    let mut times: Vec<DateTime<Utc>> = runs
        .values()
        .flat_map(|run| run.steps.keys().copied())
        .collect();
    times.sort_unstable();
    times.dedup();
    let parameters: Vec<ParameterInfo> = layout
        .map(|layout| {
            layout
                .variables
                .iter()
                .map(|v| ParameterInfo {
                    name: v.name.to_string(),
                    title: titles
                        .get(&*v.name)
                        .cloned()
                        .unwrap_or_else(|| v.title.clone()),
                    unit: v.units.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let transform = layout.map(|layout| &layout.grid.transform);
    RasterInfo {
        native_crs: match transform.map(|t| &t.crs) {
            // Lon-first degrees, OGC `storageCrs` CRS84. A projected grid
            // has no stable OGC URI, so its label leaves `storageCrs` and
            // the Common grid out, as engine-querydata's does.
            None | Some(Crs::Wgs84) => "CRS:84",
            Some(Crs::Geostationary { .. }) => "geos",
            Some(_) => "projected",
        }
        .to_string(),
        spatial_extent: transform.and_then(|t| ds_core::geo::crs84_extent(t.bbox())),
        times,
        parameter: parameters
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        unit: parameters
            .first()
            .map(|p| p.unit.clone())
            .unwrap_or_default(),
        parameters,
        vertical: layout.and_then(|layout| layout.vertical.clone()),
        grid_size: transform.map(|t| [t.width, t.height]),
        layer_subtitle: None,
        reference_times: runs.keys().flatten().copied().collect(),
    }
}

#[cfg(test)]
mod tests;
