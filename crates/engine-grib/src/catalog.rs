//! Forecast catalog: maps (reference_time, step) → file + message offsets.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};

#[cfg(test)]
use chrono::{NaiveDate, NaiveTime};

/// A single GRIB message location within a file.
#[derive(Debug, Clone)]
pub struct MessageEntry {
    /// Parameter short name (e.g., "2t", "msl").
    pub param: String,
    /// Preserved source statistic/window (wgrib2); JSON sidecars do not carry it.
    pub step_kind: crate::wgrib2_index::StepKind,
    /// Level type: "sfc" (surface), "hag" (height above ground),
    /// "pl" (pressure level), "sol" (soil), "ml" (model level), "iso" (isentropic).
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
    /// True if this message represents a near-surface field — either a surface
    /// type (`sfc`, including MSL / PBL / tropopause / entire-atmosphere in the
    /// wgrib2 canonical mapping) or a height-above-ground level at or below
    /// 100 m (covering 2 m temperature, 10 m / 80 m / 100 m winds).
    pub fn is_near_surface(&self) -> bool {
        match self.levtype.as_str() {
            "sfc" => true,
            "hag" => self.level.is_some_and(|l| l <= 100),
            _ => false,
        }
    }

    /// Priority score for "how canonical is this surface for the parameter
    /// it carries", used to pick which of several levels to probe for
    /// metadata when a short name appears at multiple levels. **Lower is
    /// more preferred.**
    ///
    /// Rationale:
    /// - `hag` at ≤ 10 m captures the conventional 2 m temperature / 10 m
    ///   wind defaults, so it outranks plain `sfc` (which for GFS often
    ///   means skin temperature or planetary boundary layer values).
    /// - `sfc` comes next (standard surface fields like surface pressure,
    ///   precipitation rate, MSL pressure).
    /// - Pressure / model / other levels come last because they represent
    ///   deliberate upper-air measurements, not a default surface view.
    pub fn surface_priority(&self) -> u8 {
        match (self.levtype.as_str(), self.level) {
            ("hag", Some(n)) if n <= 10 => 0,
            ("hag", Some(n)) if n <= 100 => 1,
            ("sfc", _) => 2,
            ("hag", _) => 3,
            ("pl", _) => 4,
            ("ml", _) => 5,
            ("iso", _) => 6,
            ("sol", _) => 7,
            _ => 8,
        }
    }
}

/// One forecast step file with its message index.
#[derive(Debug, Clone)]
pub struct StepFile {
    /// URL or path to the .grib2 file.
    pub grib_url: String,
    /// All GRIB messages in this file, indexed by (param, levtype, level).
    pub messages: Vec<MessageEntry>,
}

impl StepFile {
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
#[derive(Debug, Clone)]
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

    /// Find the step file closest to the requested valid time.
    pub fn find_step_for_time(&self, valid_time: DateTime<Utc>) -> Option<(u32, &StepFile)> {
        let target_hours = (valid_time - self.reference_time).num_hours();
        if target_hours < 0 {
            return None;
        }
        let target = target_hours as u32;

        // Exact match first
        if let Some(sf) = self.steps.get(&target) {
            return Some((target, sf));
        }

        // Find nearest step
        let mut best: Option<(u32, &StepFile)> = None;
        let mut best_diff = u32::MAX;
        for (&step, sf) in &self.steps {
            let diff = step.abs_diff(target);
            if diff < best_diff {
                best_diff = diff;
                best = Some((step, sf));
            }
        }
        best
    }
}

/// The full forecast catalog.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Forecast runs keyed by reference time (most recent last).
    pub runs: BTreeMap<DateTime<Utc>, ForecastRun>,
    /// Latest-run parameter union, rebuilt on the poll path before publication.
    parameters: Vec<(String, String, Option<u32>)>,
}

impl Catalog {
    pub fn new() -> Self {
        Self {
            runs: BTreeMap::new(),
            parameters: Vec::new(),
        }
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
        let mut times: Vec<DateTime<Utc>> = self
            .runs
            .values()
            .flat_map(|run| run.valid_times())
            .collect();
        times.sort();
        times.dedup();
        times
    }

    /// Temporal extent (earliest, latest) across all valid times.
    pub fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let times = self.all_valid_times();
        if times.is_empty() {
            return None;
        }
        Some((times[0], *times.last().unwrap()))
    }

    /// Parameters are unioned across the run: f000 commonly lacks aggregates.
    pub fn surface_params(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.all_params_with_levels()
            .into_iter()
            .filter(|(_, levtype, level)| {
                levtype == "sfc" || (levtype == "hag" && level.is_some_and(|l| l <= 100))
            })
            .map(|(param, _, _)| param)
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }

    pub fn all_params(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.all_params_with_levels()
            .into_iter()
            .map(|(p, _, _)| p)
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }

    /// All unique parameters with level info from every step in the latest run.
    pub fn all_params_with_levels(&self) -> Vec<(String, String, Option<u32>)> {
        self.parameters.clone()
    }

    /// Call after modifying runs, before publishing the immutable snapshot.
    /// Request-time metadata must not scan hundreds of forecast steps.
    pub fn refresh_parameters(&mut self) {
        self.parameters.clear();
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
    fn parse_ref_time() {
        let dt = parse_reference_time("20260405", "0000").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-04-05T00:00:00+00:00");

        let dt = parse_reference_time("20260405", "1200").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-04-05T12:00:00+00:00");
    }

    #[test]
    fn is_near_surface_cases() {
        let sfc = MessageEntry {
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
