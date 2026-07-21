//! Categorical forecast verification: contingency table + POD / FAR / CSI.
//!
//! Pixels where either grid is nodata are excluded — a nowcast must not be
//! rewarded or punished where the observation can't see.

use crate::Grid;

/// 2×2 contingency table at a fixed event threshold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Contingency {
    /// Forecast ≥ threshold and observed ≥ threshold.
    pub hits: u64,
    /// Forecast < threshold but observed ≥ threshold.
    pub misses: u64,
    /// Forecast ≥ threshold but observed < threshold.
    pub false_alarms: u64,
    /// Neither reached the threshold.
    pub correct_negatives: u64,
}

impl Contingency {
    /// Probability of detection: hits / (hits + misses). `None` if the event
    /// never occurred.
    pub fn pod(&self) -> Option<f64> {
        ratio(self.hits, self.hits + self.misses)
    }

    /// False-alarm ratio: false_alarms / (hits + false_alarms). `None` if the
    /// event was never forecast.
    pub fn far(&self) -> Option<f64> {
        ratio(self.false_alarms, self.hits + self.false_alarms)
    }

    /// Critical success index: hits / (hits + misses + false_alarms). `None`
    /// if the event neither occurred nor was forecast.
    pub fn csi(&self) -> Option<f64> {
        ratio(self.hits, self.hits + self.misses + self.false_alarms)
    }

    /// Merge another table into this one (for aggregating over frame pairs).
    pub fn merge(&mut self, other: &Contingency) {
        self.hits += other.hits;
        self.misses += other.misses;
        self.false_alarms += other.false_alarms;
        self.correct_negatives += other.correct_negatives;
    }
}

fn ratio(num: u64, den: u64) -> Option<f64> {
    (den > 0).then(|| num as f64 / den as f64)
}

/// Score `forecast` against `observed` at `threshold`, skipping pixels where
/// either grid is nodata.
pub fn score(forecast: &Grid, observed: &Grid, threshold: f32) -> Contingency {
    assert_eq!(
        (forecast.width, forecast.height),
        (observed.width, observed.height),
        "skill scoring needs equally sized grids"
    );
    let mut table = Contingency::default();
    for (&f, &o) in forecast.data.iter().zip(&observed.data) {
        if !f.is_finite() || !o.is_finite() {
            continue;
        }
        match (f >= threshold, o >= threshold) {
            (true, true) => table.hits += 1,
            (false, true) => table.misses += 1,
            (true, false) => table.false_alarms += 1,
            (false, false) => table.correct_negatives += 1,
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_and_scores_are_pinned() {
        let forecast = Grid::new(4, 1, vec![40.0, 0.0, 40.0, f32::NAN]);
        let observed = Grid::new(4, 1, vec![40.0, 40.0, 0.0, 40.0]);
        let t = score(&forecast, &observed, 20.0);
        assert_eq!(
            t,
            Contingency {
                hits: 1,
                misses: 1,
                false_alarms: 1,
                correct_negatives: 0,
            }
        );
        assert_eq!(t.pod(), Some(0.5));
        assert_eq!(t.far(), Some(0.5));
        assert_eq!(t.csi(), Some(1.0 / 3.0));
    }

    #[test]
    fn empty_denominators_are_none_not_nan() {
        let quiet = Grid::new(2, 1, vec![0.0, 0.0]);
        let t = score(&quiet, &quiet, 20.0);
        assert_eq!(t.pod(), None);
        assert_eq!(t.far(), None);
        assert_eq!(t.csi(), None);
        assert_eq!(t.correct_negatives, 2);
    }

    #[test]
    fn merge_accumulates() {
        let mut a = Contingency {
            hits: 1,
            misses: 2,
            false_alarms: 3,
            correct_negatives: 4,
        };
        a.merge(&a.clone());
        assert_eq!(a.hits, 2);
        assert_eq!(a.misses, 4);
        assert_eq!(a.false_alarms, 6);
        assert_eq!(a.correct_negatives, 8);
    }
}
