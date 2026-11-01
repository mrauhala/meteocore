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

    /// Count one pixel: whether the forecast and the observation reached
    /// the threshold there.
    #[inline]
    fn record(&mut self, forecast: bool, observed: bool) {
        match (forecast, observed) {
            (true, true) => self.hits += 1,
            (false, true) => self.misses += 1,
            (true, false) => self.false_alarms += 1,
            (false, false) => self.correct_negatives += 1,
        }
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
        table.record(f >= threshold, o >= threshold);
    }
    table
}

/// Score `forecast` and a `reference` forecast (persistence) against one
/// observation at each of `thresholds`: one `(forecast, reference)` table
/// pair per threshold, in order.
///
/// Only pixels where all three grids have data count, so the two tables
/// are drawn from one sample. That is what makes them comparable at deeper
/// leads: the extrapolation is nodata where its trajectories leave the
/// domain (an inflow strip that widens with the lead), and scoring
/// persistence over pixels the nowcast was excused from would compare two
/// different populations.
pub fn score_with_reference(
    forecast: &Grid,
    reference: &Grid,
    observed: &Grid,
    thresholds: &[f32],
) -> Vec<(Contingency, Contingency)> {
    let dims = |g: &Grid| (g.width, g.height);
    assert!(
        dims(forecast) == dims(observed) && dims(reference) == dims(observed),
        "skill scoring needs equally sized grids"
    );
    let mut tables = vec![(Contingency::default(), Contingency::default()); thresholds.len()];
    for ((&f, &r), &o) in forecast
        .data
        .iter()
        .zip(&reference.data)
        .zip(&observed.data)
    {
        if !(f.is_finite() && r.is_finite() && o.is_finite()) {
            continue;
        }
        for (&t, (ft, rt)) in thresholds.iter().zip(tables.iter_mut()) {
            ft.record(f >= t, o >= t);
            rt.record(r >= t, o >= t);
        }
    }
    tables
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

    /// Hand-checked at two thresholds: pixel 4 is nodata in the forecast
    /// only, so it must drop out of the persistence table too.
    #[test]
    fn reference_tables_share_one_mask_and_every_threshold() {
        //                          0     1     2     3     4         5
        let forecast = Grid::new(6, 1, vec![40.0, 25.0, 25.0, 0.0, f32::NAN, 40.0]);
        let persistence = Grid::new(6, 1, vec![40.0, 40.0, 0.0, 0.0, 40.0, 0.0]);
        let observed = Grid::new(6, 1, vec![40.0, 40.0, 25.0, 25.0, 40.0, 0.0]);
        let tables = score_with_reference(&forecast, &persistence, &observed, &[20.0, 35.0]);
        let table = |hits, misses, false_alarms, correct_negatives| Contingency {
            hits,
            misses,
            false_alarms,
            correct_negatives,
        };
        // >= 20: forecast hits 0,1,2, misses 3, false alarm 5.
        //        persistence hits 0,1, misses 2,3, correct negative 5.
        assert_eq!(tables[0], (table(3, 1, 1, 0), table(2, 2, 0, 1)));
        // >= 35: forecast hits 0, misses 1, false alarm 5, negatives 2,3.
        //        persistence hits 0,1, negatives 2,3,5.
        assert_eq!(tables[1], (table(1, 1, 1, 2), table(2, 0, 0, 3)));
        assert_eq!(tables[0].0.csi(), Some(3.0 / 5.0));
        assert_eq!(tables[0].1.csi(), Some(2.0 / 4.0));
        assert_eq!(tables[1].0.csi(), Some(1.0 / 3.0));
        assert_eq!(tables[1].1.csi(), Some(1.0));
        // Scored alone, persistence would also count pixel 4 as a hit.
        assert_eq!(score(&persistence, &observed, 20.0).hits, 3);
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
