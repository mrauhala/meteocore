//! The snapshot a poll publishes, and the one selection of a run and step
//! that the render and both cache-key resolvers go through.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::geo::Crs;
use ds_core::map_engine::{ParameterInfo, RasterInfo};
use ds_core::resample::SourceGrid;
use netcdf_reader::NcFile;

use crate::ingest::{FieldRead, FileGrid, Variable};

/// What a file is on disk: a file whose size or modification time changes
/// is another file, ingested again (#1067 decision 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Identity {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// A catalogued file, open for as long as the catalog keeps it. Requests
/// read through its handle and never open a file.
pub(crate) struct OpenFile {
    /// Unique in the process: the field cache's key ([`crate::field`]).
    pub id: u64,
    pub path: PathBuf,
    pub identity: Identity,
    pub nc: NcFile,
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
        vertical: None,
        grid_size: transform.map(|t| [t.width, t.height]),
        layer_subtitle: None,
        reference_times: runs.keys().flatten().copied().collect(),
    }
}

#[cfg(test)]
mod tests;
