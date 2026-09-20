//! Forecast catalog: maps (reference_time, step) → file + message offsets.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::config::GribLevelType;

/// The level identity behind a public parameter name. Keep the level type:
/// pressure at 2 hPa and height at 2 m must never name the same message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParameterKey {
    pub param: String,
    pub levtype: String,
    pub level: Option<u32>,
}

pub type ParameterKeys = BTreeMap<String, ParameterKey>;

fn is_named_single_level(levtype: &str) -> bool {
    matches!(
        levtype,
        "sfc"
            | "msl"
            | "atmosphere"
            | "atmosphere_layer"
            | "tropopause"
            | "max_wind"
            | "convective_cloud_bottom"
            | "convective_cloud_top"
            | "convective_cloud"
            | "pbl"
            | "cloud_ceiling"
            | "zero_isotherm"
            | "highest_freezing"
    )
}

fn is_near_surface(levtype: &str, level: Option<u32>) -> bool {
    match levtype {
        "sfc" | "msl" | "atmosphere" | "atmosphere_layer" | "pbl" => true,
        "hag" => level.is_some_and(|l| l <= 100),
        _ => false,
    }
}

impl ParameterKey {
    pub fn matches(&self, message: &MessageEntry) -> bool {
        self.param == message.param
            && self.levtype == message.levtype
            && self.level == message.level
    }
}

#[cfg(test)]
use chrono::{NaiveDate, NaiveTime};

/// A single GRIB message location within a file.
#[derive(Debug, Clone, Hash)]
pub struct MessageEntry {
    /// Origin when a forecast step contains messages from multiple files.
    /// Unset in a freshly parsed sidecar; scanning attaches the actual path.
    pub source_url: Option<Arc<str>>,
    /// Parameter short name (e.g., "2t", "msl").
    pub param: String,
    /// Preserved source statistic/window (wgrib2); JSON sidecars do not carry it.
    pub step_kind: crate::wgrib2_index::StepKind,
    /// Level type: "sfc" (surface), "hag" (height above ground),
    /// "pl" (pressure level), "ml" (model level), "iso" (isentropic), or a
    /// distinct named single level. Wgrib2 soil layers use "sol:top-bottom"
    /// in metres to retain both boundaries; ECMWF types are unchanged.
    pub levtype: String,
    /// Level value (e.g., 850 for pressure levels). None for surface.
    pub level: Option<u32>,
    /// Byte offset within the GRIB file.
    pub offset: u64,
    /// Byte length of this GRIB message.
    ///
    /// `None` means "last record in the file, length not yet known" — this
    /// happens for wgrib2 index files, which only carry offsets. The engine
    /// resolves the length via a HEAD request on the data file the first
    /// time someone actually fetches this message, not during scan.
    pub length: Option<u64>,
}

impl MessageEntry {
    pub fn level_type(&self) -> Option<GribLevelType> {
        match self.levtype.as_str() {
            "hag" => Some(GribLevelType::Single),
            named if is_named_single_level(named) => Some(GribLevelType::Single),
            "pl" if self.level.is_some() => Some(GribLevelType::Pressure),
            "ml" if self.level.is_some() => Some(GribLevelType::Model),
            _ => None,
        }
    }

    pub fn key(&self) -> ParameterKey {
        ParameterKey {
            param: self.param.clone(),
            levtype: self.levtype.clone(),
            level: self.level,
        }
    }

    fn preference(&self) -> (u8, Option<u32>, &str) {
        (self.surface_priority(), self.level, &self.levtype)
    }

    /// Near-surface and column products eligible for default queries. Named
    /// upper-air fields such as tropopause remain single-level products, but
    /// must not take the place of surface or whole-atmosphere fields.
    pub fn is_near_surface(&self) -> bool {
        is_near_surface(&self.levtype, self.level)
    }

    /// Priority score for "how canonical is this surface for the parameter
    /// it carries", used to pick which of several levels to probe for
    /// metadata when a short name appears at multiple levels. **Lower is
    /// more preferred.**
    ///
    /// Rationale:
    /// - `hag` at ≤ 10 m captures the conventional 2 m temperature / 10 m
    ///   wind defaults, so it outranks plain `sfc` (e.g. skin temperature).
    /// - Ground, MSL and whole-atmosphere fields outrank named upper-air
    ///   surfaces/layers, independent of their order in the source index.
    /// - Pressure / model / other levels come last because they represent
    ///   deliberate upper-air measurements, not a default surface view.
    pub fn surface_priority(&self) -> u8 {
        match (self.levtype.as_str(), self.level) {
            ("hag", Some(n)) if n <= 10 => 0,
            ("hag", Some(n)) if n <= 100 => 1,
            ("sfc", _) => 2,
            ("msl", _) => 3,
            ("atmosphere" | "atmosphere_layer", _) => 4,
            ("hag", _) => 5,
            ("pbl", _) => 6,
            (named, _) if is_named_single_level(named) => 7,
            ("pl", _) => 8,
            ("ml", _) => 9,
            ("iso", _) => 10,
            (soil, _) if soil == "sol" || soil.starts_with("sol:") => 11,
            _ => 12,
        }
    }
}

/// Sidecars can repeat a catalog key at distinct offsets (including GFS
/// accumulation records). Report these without assuming their payloads are
/// equivalent or inventing a scientific discriminator absent from the index.
pub(crate) fn duplicate_message_keys<'a>(
    messages: impl IntoIterator<Item = &'a MessageEntry>,
) -> impl Iterator<Item = (&'a MessageEntry, &'a MessageEntry)> {
    let mut first = HashMap::new();
    messages.into_iter().filter_map(move |message| {
        let key = (&message.param, &message.levtype, message.level);
        match first.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(message);
                None
            }
            std::collections::hash_map::Entry::Occupied(entry) => Some((*entry.get(), message)),
        }
    })
}

/// One forecast step file with its message index.
#[derive(Debug, Clone, Hash)]
pub struct StepFile {
    /// URL or path to the .grib2 file.
    pub grib_url: String,
    /// All GRIB messages in this file, indexed by (param, levtype, level).
    pub messages: Vec<MessageEntry>,
}

impl StepFile {
    pub fn message_url<'a>(&'a self, entry: &'a MessageEntry) -> &'a str {
        entry.source_url.as_deref().unwrap_or(&self.grib_url)
    }

    /// Preserve the existing surface default when newly supported acc/ave
    /// records precede it in an index. An aggregate-only collection still
    /// has a useful default, as does a collection containing only upper air.
    pub fn default_message(&self) -> Option<&MessageEntry> {
        self.messages
            .iter()
            .find(|m| {
                m.is_near_surface()
                    && !matches!(
                        m.step_kind,
                        crate::wgrib2_index::StepKind::Accumulation { .. }
                            | crate::wgrib2_index::StepKind::Average { .. }
                    )
            })
            .or_else(|| self.messages.iter().find(|m| m.is_near_surface()))
            .or_else(|| self.messages.first())
    }

    /// Find a message by parameter short name and optional level.
    /// For surface parameters, level should be None.
    pub fn find_message(&self, param: &str, level: Option<u32>) -> Option<&MessageEntry> {
        self.messages
            .iter()
            .find(|m| m.param == param && m.level == level)
    }

    /// List all unique parameter names in this step file.
    pub fn param_names(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut names = Vec::new();
        for m in &self.messages {
            if seen.insert(&m.param) {
                names.push(m.param.clone());
            }
        }
        names
    }
}

/// A single forecast run (e.g., 00z on 2026-04-05).
#[derive(Debug, Clone, Hash)]
pub struct ForecastRun {
    /// Model reference time (analysis time).
    pub reference_time: DateTime<Utc>,
    /// Available forecast steps, keyed by step in hours.
    pub steps: BTreeMap<u32, StepFile>,
}

impl ForecastRun {
    /// Valid times for all available steps.
    pub fn valid_times(&self) -> Vec<DateTime<Utc>> {
        self.steps
            .keys()
            .map(|&step| self.reference_time + chrono::Duration::hours(i64::from(step)))
            .collect()
    }

    /// Find the closest step within this run's available valid-time extent.
    pub fn find_step_for_time(&self, valid_time: DateTime<Utc>) -> Option<(u32, &StepFile)> {
        let (&first, _) = self.steps.first_key_value()?;
        let (&last, _) = self.steps.last_key_value()?;
        let first_time = self.reference_time + chrono::Duration::hours(i64::from(first));
        let last_time = self.reference_time + chrono::Duration::hours(i64::from(last));
        if valid_time < first_time || valid_time > last_time {
            return None;
        }
        // Only the preceding and following steps can be closest. Compare
        // actual instants (including fractional hours), preferring the earlier
        // step on ties, without scanning every step on each render request.
        let hour = (valid_time - self.reference_time).num_hours() as u32;
        let before = self.steps.range(..=hour).next_back();
        let after = self
            .steps
            .range((std::ops::Bound::Excluded(hour), std::ops::Bound::Unbounded))
            .next();
        before
            .into_iter()
            .chain(after)
            .min_by_key(|(&step, _)| {
                let time = self.reference_time + chrono::Duration::hours(i64::from(step));
                (time - valid_time).abs()
            })
            .map(|(&step, file)| (step, file))
    }
}

/// The full forecast catalog.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Forecast runs keyed by reference time (most recent last).
    pub runs: BTreeMap<DateTime<Utc>, ForecastRun>,
    /// Fingerprint for rendered caches, computed on publication from the
    /// ordered message identities (including origins, offsets and levels).
    pub content_version: u64,
    /// Latest-run parameter union, rebuilt on the poll path before publication.
    parameters: Vec<(String, String, Option<u32>)>,
    /// Canonical levels per run, selected once on publication. A missing
    /// canonical level in a step is missing data, not a switch to upper air.
    parameter_keys: BTreeMap<DateTime<Utc>, ParameterKeys>,
    /// Built once on the scan path. Child catalogs contain no further children.
    pub families: BTreeMap<GribLevelType, Arc<Catalog>>,
    /// Per-run level union, also precomputed for vertical query selection.
    pub levels: BTreeMap<DateTime<Utc>, Vec<f64>>,
    pub vertical_levels: Vec<f64>,
    valid_times: Vec<DateTime<Utc>>,
    param_names: Vec<String>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the latest (most recent) forecast run.
    pub fn latest_run(&self) -> Option<&ForecastRun> {
        self.runs.values().next_back()
    }

    /// Find the best run+step for a requested valid time.
    /// Prefers the most recent run that has a step close to the requested time.
    pub fn find_for_time(&self, valid_time: DateTime<Utc>) -> Option<(u32, &StepFile)> {
        // Try runs from most recent to oldest
        for run in self.runs.values().rev() {
            if let Some(result) = run.find_step_for_time(valid_time) {
                return Some(result);
            }
        }
        None
    }

    /// All unique valid times across all runs, sorted.
    pub fn all_valid_times(&self) -> Vec<DateTime<Utc>> {
        self.valid_times.clone()
    }

    /// Temporal extent (earliest, latest) across all valid times.
    pub fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.valid_times
            .first()
            .copied()
            .zip(self.valid_times.last().copied())
    }

    /// Parameters are unioned across the run: f000 commonly lacks aggregates.
    pub fn surface_params(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.all_params_with_levels()
            .into_iter()
            .filter(|(_, levtype, level)| is_near_surface(levtype, *level))
            .map(|(param, _, _)| param)
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }

    pub fn all_params(&self) -> Vec<String> {
        self.param_names.clone()
    }

    /// All unique parameters with level info from every step in the latest run.
    pub fn all_params_with_levels(&self) -> Vec<(String, String, Option<u32>)> {
        self.parameters.clone()
    }

    pub fn parameter_keys(&self, reference_time: &DateTime<Utc>) -> Option<&ParameterKeys> {
        self.parameter_keys.get(reference_time)
    }

    /// Call after modifying runs, before publishing the immutable snapshot.
    /// Request-time metadata and cache keys must not scan forecast steps.
    pub fn refresh_metadata(&mut self) {
        self.valid_times = self
            .runs
            .values()
            .flat_map(ForecastRun::valid_times)
            .collect();
        self.valid_times.sort_unstable();
        self.valid_times.dedup();
        // This keys process-local caches, not persistent IDs or public ETags.
        // A content hash preserves hits on no-op rebuilds and cannot reset to
        // an old revision counter when an evicted family reappears.
        let mut hasher = DefaultHasher::new();
        self.runs.hash(&mut hasher);
        self.content_version = hasher.finish().max(1);
        self.parameters.clear();
        self.param_names.clear();
        self.parameter_keys = self
            .runs
            .iter()
            .map(|(&reference_time, run)| {
                let mut chosen: BTreeMap<&str, &MessageEntry> = BTreeMap::new();
                for message in run.steps.values().flat_map(|sf| &sf.messages) {
                    chosen
                        .entry(&message.param)
                        .and_modify(|current| {
                            if message.preference() < current.preference() {
                                *current = message;
                            }
                        })
                        .or_insert(message);
                }
                (
                    reference_time,
                    chosen
                        .into_iter()
                        .map(|(name, m)| (name.to_owned(), m.key()))
                        .collect(),
                )
            })
            .collect();
        let Some(run) = self.runs.values().next_back() else {
            return;
        };
        let mut seen = HashMap::new();
        for m in run.steps.values().flat_map(|sf| &sf.messages) {
            let key = (&m.param, &m.levtype, m.level);
            if seen.insert(key, ()).is_none() {
                self.parameters
                    .push((m.param.clone(), m.levtype.clone(), m.level));
            }
        }
        let mut seen = HashSet::new();
        self.param_names = self
            .parameters
            .iter()
            .map(|(name, _, _)| name)
            .filter(|name| seen.insert(*name))
            .cloned()
            .collect();
    }

    pub fn refresh_families(&mut self, enabled: &[GribLevelType]) {
        self.families.clear();
        for &family in enabled {
            let mut catalog = Catalog::new();
            for (&rt, run) in &self.runs {
                let mut steps = BTreeMap::new();
                let mut levels = std::collections::BTreeSet::new();
                for (&step, file) in &run.steps {
                    let messages: Vec<_> = file
                        .messages
                        .iter()
                        .filter(|m| m.level_type() == Some(family))
                        .cloned()
                        .collect();
                    if messages.is_empty() {
                        continue;
                    }
                    if family != GribLevelType::Single {
                        levels.extend(messages.iter().filter_map(|m| m.level));
                    }
                    steps.insert(
                        step,
                        StepFile {
                            grib_url: file.grib_url.clone(),
                            messages,
                        },
                    );
                }
                if !steps.is_empty() {
                    catalog.runs.insert(
                        rt,
                        ForecastRun {
                            reference_time: rt,
                            steps,
                        },
                    );
                    // Bottom first: largest pressure / largest model level.
                    let levels: Vec<_> = levels.into_iter().rev().map(f64::from).collect();
                    catalog.levels.insert(rt, levels);
                }
            }
            let levels: std::collections::BTreeSet<_> = catalog
                .levels
                .values()
                .flatten()
                .map(|&v| v as u32)
                .collect();
            catalog.vertical_levels = levels.into_iter().rev().map(f64::from).collect();
            catalog.refresh_metadata();
            self.families.insert(family, Arc::new(catalog));
        }
    }

    /// Apply max_runs eviction: keep only the N most recent runs.
    pub fn evict(&mut self, max_runs: usize) {
        while self.runs.len() > max_runs {
            self.runs.pop_first();
        }
    }
}

#[cfg(test)]
fn parse_reference_time(date: &str, time: &str) -> Option<DateTime<Utc>> {
    let nd = NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
    let nt = NaiveTime::parse_from_str(time, "%H%M").ok()?;
    Some(nd.and_time(nt).and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_step_respects_actual_extent_and_fractional_hours() {
        let reference_time = parse_reference_time("20260405", "0000").unwrap();
        let run = ForecastRun {
            reference_time,
            steps: [3, 6]
                .into_iter()
                .map(|step| {
                    (
                        step,
                        StepFile {
                            grib_url: "unused".into(),
                            messages: vec![],
                        },
                    )
                })
                .collect(),
        };
        let at = |minutes| reference_time + chrono::Duration::minutes(minutes);
        assert!(run.find_step_for_time(at(179)).is_none());
        assert!(run.find_step_for_time(at(361)).is_none());
        assert_eq!(run.find_step_for_time(at(180)).unwrap().0, 3);
        assert_eq!(run.find_step_for_time(at(270)).unwrap().0, 3); // tie
        assert_eq!(run.find_step_for_time(at(271)).unwrap().0, 6);
        assert_eq!(run.find_step_for_time(at(360)).unwrap().0, 6);
    }

    #[test]
    fn parse_ref_time() {
        let dt = parse_reference_time("20260405", "0000").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-04-05T00:00:00+00:00");

        let dt = parse_reference_time("20260405", "1200").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-04-05T12:00:00+00:00");
    }

    #[test]
    fn is_near_surface_cases() {
        let sfc = MessageEntry {
            source_url: None,
            step_kind: crate::wgrib2_index::StepKind::Instant,
            param: "msl".into(),
            levtype: "sfc".into(),
            level: None,
            offset: 0,
            length: Some(1),
        };
        assert!(sfc.is_near_surface());

        let hag2 = MessageEntry {
            levtype: "hag".into(),
            level: Some(2),
            ..sfc.clone()
        };
        assert!(hag2.is_near_surface());

        let hag100 = MessageEntry {
            levtype: "hag".into(),
            level: Some(100),
            ..sfc.clone()
        };
        assert!(hag100.is_near_surface());

        let hag500 = MessageEntry {
            levtype: "hag".into(),
            level: Some(500),
            ..sfc.clone()
        };
        assert!(!hag500.is_near_surface());

        let pl850 = MessageEntry {
            levtype: "pl".into(),
            level: Some(850),
            ..sfc.clone()
        };
        assert!(!pl850.is_near_surface());

        let ml1 = MessageEntry {
            levtype: "ml".into(),
            level: Some(1),
            ..sfc.clone()
        };
        assert!(!ml1.is_near_surface());
    }

    #[test]
    fn catalog_eviction() {
        use chrono::Datelike;
        let mut catalog = Catalog::new();
        for day in 1..=5 {
            let dt = parse_reference_time(&format!("202604{day:02}"), "0000").unwrap();
            catalog.runs.insert(
                dt,
                ForecastRun {
                    reference_time: dt,
                    steps: BTreeMap::new(),
                },
            );
        }
        assert_eq!(catalog.runs.len(), 5);
        catalog.evict(2);
        assert_eq!(catalog.runs.len(), 2);
        // Should keep the two most recent
        let keys: Vec<_> = catalog.runs.keys().collect();
        assert_eq!(keys[0].day(), 4);
        assert_eq!(keys[1].day(), 5);
    }
}
