//! Skill at fixed leads (#648): forecasts held until the analysis for their
//! valid time arrives, then scored against it next to persistence.
//!
//! Each generation also scores the previous one's lead-1 frame (the
//! `nowcast_lead1_*` gauges). At 5 minutes and `min_echo` persistence
//! already scores about 0.8 there, so that is a heartbeat, not a
//! diagnostic. Deeper leads cannot be scored from retained generations:
//! with `max_generations = 2` the +60 min forecast is gone long before its
//! analysis arrives.
//!
//! [`SkillRing`] keeps one forecast in flight per lead in
//! [`SKILL_LEADS_MIN`], independently of generation retention. A slot holds
//! the lead frame, the analysis it was issued from (persistence: the
//! forecast that nothing moves, for the same valid time) and the grid both
//! are on. When an analysis arrives at the slot's valid time, both frames
//! are scored against it at every threshold in [`SKILL_THRESHOLDS`], and the
//! slot refills from that generation. Each lead is therefore scored once
//! per lead time: a +60 min forecast once an hour. Holding every
//! generation's forecast instead would keep lead ÷ cadence frames per lead,
//! 12 at +60 min and a 5-minute cadence.
//!
//! Memory: at most one forecast and one persistence frame per lead, in the
//! engine's own frame type (1 byte per pixel for a byte source). Slots
//! filled by the same generation share one persistence frame.
//!
//! Like `skill`, this module does no I/O: the engine hands it each
//! generation's frames on the poll runtime.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::skill::score_with_reference;
use crate::Grid;

/// Leads scored, in minutes. Fixed rather than configured: every lead holds
/// up to two frames and adds series to `/metrics`.
pub const SKILL_LEADS_MIN: [i64; 2] = [30, 60];

/// Event thresholds, in the source's display units (dBZ for radar): rain,
/// and convective cores (the tracker's cell threshold).
pub const SKILL_THRESHOLDS: [f32; 2] = [20.0, 35.0];

const LEADS: usize = SKILL_LEADS_MIN.len();
const THRESHOLDS: usize = SKILL_THRESHOLDS.len();

/// One [`LeadSkill`] per lead, in [`SKILL_LEADS_MIN`] order.
pub type LeadSkills = [LeadSkill; LEADS];

/// A stored frame the ring can hold and score.
pub trait SkillFrame {
    /// Physical values on a `width` × `height` grid, `NaN` = nodata.
    fn to_grid(&self, width: usize, height: usize) -> Grid;
    /// Heap bytes the frame holds.
    fn heap_bytes(&self) -> u64;
}

impl SkillFrame for Grid {
    fn to_grid(&self, _width: usize, _height: usize) -> Grid {
        self.clone()
    }

    fn heap_bytes(&self) -> u64 {
        (self.data.capacity() * std::mem::size_of::<f32>()) as u64
    }
}

/// Why a forecast was dropped unscored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Its valid time passed without an analysis at exactly that time: the
    /// source skipped the frame, or the generation for it failed.
    NoAnalysis,
    /// The analyses moved to another grid (source extent or size changed),
    /// so the forecast can never be compared pixel for pixel.
    GridMismatch,
}

impl SkipReason {
    /// The `reason` label of `nowcast_lead_skipped_pairs_total`.
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::NoAnalysis => "no_analysis",
            SkipReason::GridMismatch => "grid_mismatch",
        }
    }
}

/// What the ring reports for one lead: counts since the engine was built,
/// and the latest scored pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LeadSkill {
    pub lead_minutes: i64,
    pub scored_pairs: u64,
    pub skipped_no_analysis: u64,
    pub skipped_grid_mismatch: u64,
    /// Per threshold, in [`SKILL_THRESHOLDS`] order: `(nowcast,
    /// persistence)` CSI × 1000 of the latest scored pair. `None` before the
    /// first pair, and when the latest pair left either CSI undefined (no
    /// echo at that threshold in the forecast and the analysis, or in
    /// persistence and the analysis). The two are one measurement: they are
    /// set together or not at all, and an undefined pair clears the last
    /// one rather than serving it as current — at 35 dBZ a dry spell would
    /// otherwise hold a weeks-old score.
    pub csi_permille: [Option<(u64, u64)>; THRESHOLDS],
}

/// One event of [`SkillRing::advance`], for the engine's log.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A forecast was scored. `csi` is `(nowcast, persistence)` per
    /// threshold.
    Scored {
        lead_minutes: i64,
        issued: DateTime<Utc>,
        valid: DateTime<Utc>,
        csi: [(Option<f64>, Option<f64>); THRESHOLDS],
    },
    /// A forecast was dropped unscored.
    Skipped {
        lead_minutes: i64,
        valid: DateTime<Utc>,
        reason: SkipReason,
    },
    /// No frame of the generation is valid at this lead (horizon shorter
    /// than the lead, or a step that does not divide it). Reported once,
    /// until the lead can be issued again.
    Unissuable { lead_minutes: i64 },
}

/// One generation, as the ring sees it.
pub struct GenerationFrames<'a, F, G> {
    /// Valid times: `[0]` is the analysis (the anchor), then the leads.
    pub times: &'a [DateTime<Utc>],
    /// One frame per valid time.
    pub frames: &'a [F],
    /// The grid the frames are on; frames on unequal grids are never
    /// compared.
    pub geom: G,
    pub width: usize,
    pub height: usize,
}

struct Pending<F, G> {
    issued: DateTime<Utc>,
    valid: DateTime<Utc>,
    geom: G,
    forecast: F,
    /// The analysis the forecast was issued from. Shared by the slots one
    /// generation filled.
    persistence: Arc<F>,
}

/// One forecast in flight per lead (see the module docs).
pub struct SkillRing<F, G> {
    slots: [Option<Pending<F, G>>; LEADS],
    stats: LeadSkills,
    unissuable_reported: [bool; LEADS],
}

impl<F: SkillFrame + Clone, G: PartialEq + Copy> Default for SkillRing<F, G> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: SkillFrame + Clone, G: PartialEq + Copy> SkillRing<F, G> {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            stats: std::array::from_fn(|k| LeadSkill {
                lead_minutes: SKILL_LEADS_MIN[k],
                ..LeadSkill::default()
            }),
            unissuable_reported: [false; LEADS],
        }
    }

    /// Feed one new generation: score or drop the forecasts its analysis
    /// settles, then fill the empty slots from its leads.
    ///
    /// A forecast on another grid is dropped at once (it can never verify);
    /// one valid at exactly the analysis time is scored; one whose valid
    /// time has passed is dropped as [`SkipReason::NoAnalysis`]. Later
    /// forecasts wait.
    pub fn advance(&mut self, generation: GenerationFrames<'_, F, G>) -> Vec<Outcome> {
        let GenerationFrames {
            times,
            frames,
            geom,
            width,
            height,
        } = generation;
        assert_eq!(times.len(), frames.len(), "one frame per valid time");
        let mut outcomes = Vec::new();
        let (Some(&anchor), Some(analysis)) = (times.first(), frames.first()) else {
            return outcomes;
        };
        let mut observed: Option<Grid> = None;
        for (k, &lead_minutes) in SKILL_LEADS_MIN.iter().enumerate() {
            let Some(pending) = &self.slots[k] else {
                continue;
            };
            let skip = if pending.geom != geom {
                Some(SkipReason::GridMismatch)
            } else if pending.valid < anchor {
                Some(SkipReason::NoAnalysis)
            } else if pending.valid > anchor {
                continue;
            } else {
                None
            };
            let pending = self.slots[k].take().expect("slot checked above");
            let stats = &mut self.stats[k];
            match skip {
                Some(reason) => {
                    match reason {
                        SkipReason::NoAnalysis => stats.skipped_no_analysis += 1,
                        SkipReason::GridMismatch => stats.skipped_grid_mismatch += 1,
                    }
                    outcomes.push(Outcome::Skipped {
                        lead_minutes,
                        valid: pending.valid,
                        reason,
                    });
                }
                None => {
                    let observed = observed.get_or_insert_with(|| analysis.to_grid(width, height));
                    let tables = score_with_reference(
                        &pending.forecast.to_grid(width, height),
                        &pending.persistence.to_grid(width, height),
                        observed,
                        &SKILL_THRESHOLDS,
                    );
                    let mut csi = [(None, None); THRESHOLDS];
                    for (j, (nowcast, persistence)) in tables.iter().enumerate() {
                        csi[j] = (nowcast.csi(), persistence.csi());
                        stats.csi_permille[j] = match csi[j] {
                            (Some(n), Some(p)) => Some((permille(n), permille(p))),
                            _ => None,
                        };
                    }
                    stats.scored_pairs += 1;
                    outcomes.push(Outcome::Scored {
                        lead_minutes,
                        issued: pending.issued,
                        valid: pending.valid,
                        csi,
                    });
                }
            }
        }

        let mut persistence: Option<Arc<F>> = None;
        for (k, &lead_minutes) in SKILL_LEADS_MIN.iter().enumerate() {
            if self.slots[k].is_some() {
                continue;
            }
            let valid = anchor + Duration::minutes(lead_minutes);
            let Some(i) = times.iter().position(|&t| t == valid) else {
                if !self.unissuable_reported[k] {
                    self.unissuable_reported[k] = true;
                    outcomes.push(Outcome::Unissuable { lead_minutes });
                }
                continue;
            };
            self.unissuable_reported[k] = false;
            let persistence = persistence
                .get_or_insert_with(|| Arc::new(analysis.clone()))
                .clone();
            self.slots[k] = Some(Pending {
                issued: anchor,
                valid,
                geom,
                forecast: frames[i].clone(),
                persistence,
            });
        }
        outcomes
    }

    /// Bytes of the frames the ring holds, a shared persistence frame once.
    pub fn heap_bytes(&self) -> u64 {
        let mut total = 0;
        let mut counted: Vec<*const F> = Vec::with_capacity(LEADS);
        for pending in self.slots.iter().flatten() {
            total += pending.forecast.heap_bytes();
            let shared = Arc::as_ptr(&pending.persistence);
            if !counted.contains(&shared) {
                counted.push(shared);
                total += pending.persistence.heap_bytes();
            }
        }
        total
    }

    /// Per-lead counts and latest scores, in [`SKILL_LEADS_MIN`] order.
    pub fn stats(&self) -> LeadSkills {
        self.stats
    }

    /// The forecasts in flight: `(lead_minutes, issued, valid)`.
    pub fn pending(&self) -> Vec<(i64, DateTime<Utc>, DateTime<Utc>)> {
        self.slots
            .iter()
            .zip(SKILL_LEADS_MIN)
            .filter_map(|(slot, lead)| slot.as_ref().map(|p| (lead, p.issued, p.valid)))
            .collect()
    }
}

fn permille(csi: f64) -> u64 {
    (csi * 1000.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 6;

    fn t0() -> DateTime<Utc> {
        "2026-07-20T12:00:00Z".parse().unwrap()
    }

    fn minutes(m: i64) -> DateTime<Utc> {
        t0() + Duration::minutes(m)
    }

    fn row(values: [f32; W]) -> Grid {
        Grid::new(W, 1, values.to_vec())
    }

    /// A generation at `anchor` with 5-minute leads to +`horizon` minutes;
    /// every frame is `fill` unless `frames` names it by lead minute.
    fn generation(
        anchor: i64,
        horizon: i64,
        fill: Grid,
        frames: &[(i64, Grid)],
    ) -> (Vec<DateTime<Utc>>, Vec<Grid>) {
        (0..=horizon / 5)
            .map(|i| {
                let lead = 5 * i;
                let frame = frames
                    .iter()
                    .find(|(m, _)| *m == lead)
                    .map_or_else(|| fill.clone(), |(_, g)| g.clone());
                (minutes(anchor + lead), frame)
            })
            .unzip()
    }

    fn feed(
        ring: &mut SkillRing<Grid, u8>,
        geom: u8,
        (times, frames): &(Vec<DateTime<Utc>>, Vec<Grid>),
    ) -> Vec<Outcome> {
        ring.advance(GenerationFrames {
            times,
            frames,
            geom,
            width: W,
            height: 1,
        })
    }

    fn quiet() -> Grid {
        row([0.0; W])
    }

    /// Hand-checked: the +30 forecast and its persistence frame (the
    /// analysis it was issued from) against the analysis 30 minutes later,
    /// at both thresholds. Pixel 4 is nodata in the forecast, so it drops
    /// out of the persistence table as well.
    #[test]
    fn a_forecast_is_scored_at_its_valid_time_against_persistence() {
        let issued = row([40.0, 40.0, 0.0, 0.0, 40.0, 0.0]);
        let forecast = row([40.0, 25.0, 25.0, 0.0, f32::NAN, 40.0]);
        let verifying = row([40.0, 40.0, 25.0, 25.0, 40.0, 0.0]);
        let mut ring = SkillRing::new();
        assert!(feed(&mut ring, 1, &generation(0, 60, issued, &[(30, forecast)])).is_empty());
        // Generations in between change nothing.
        for anchor in (5..30).step_by(5) {
            assert!(feed(&mut ring, 1, &generation(anchor, 60, quiet(), &[])).is_empty());
        }
        let outcomes = feed(&mut ring, 1, &generation(30, 60, verifying, &[]));
        // >= 20: nowcast 3 hits, 1 miss, 1 false alarm; persistence 2 hits,
        // 2 misses. >= 35: nowcast 1 hit, 1 miss, 1 false alarm;
        // persistence 2 hits.
        assert_eq!(
            outcomes,
            vec![Outcome::Scored {
                lead_minutes: 30,
                issued: t0(),
                valid: minutes(30),
                csi: [(Some(3.0 / 5.0), Some(0.5)), (Some(1.0 / 3.0), Some(1.0))],
            }]
        );
        let [lead30, lead60] = ring.stats();
        assert_eq!(
            lead30,
            LeadSkill {
                lead_minutes: 30,
                scored_pairs: 1,
                skipped_no_analysis: 0,
                skipped_grid_mismatch: 0,
                csi_permille: [Some((600, 500)), Some((333, 1000))],
            }
        );
        assert_eq!(lead60.scored_pairs, 0, "+60 is still in flight");
    }

    /// Persistence is the analysis the forecast was ISSUED from, 60 minutes
    /// before its valid time — not the analysis of the generation before the
    /// verifying one, which is the lead-1 baseline.
    #[test]
    fn persistence_is_scored_at_the_same_lead() {
        let echo = |x: usize| {
            let mut v = [0.0; W];
            v[x] = 40.0;
            row(v)
        };
        let mut ring = SkillRing::new();
        feed(&mut ring, 1, &generation(0, 60, echo(0), &[(60, echo(5))]));
        for anchor in (5..60).step_by(5) {
            // The echo creeps east; at 55 it sits where it will be at 60.
            let x = if anchor == 55 {
                5
            } else {
                1 + (anchor as usize / 15)
            };
            feed(&mut ring, 1, &generation(anchor, 60, echo(x), &[]));
        }
        let outcomes = feed(&mut ring, 1, &generation(60, 60, echo(5), &[]));
        let scored = outcomes
            .iter()
            .find(|o| {
                matches!(
                    o,
                    Outcome::Scored {
                        lead_minutes: 60,
                        ..
                    }
                )
            })
            .expect("+60 scored");
        let Outcome::Scored { issued, csi, .. } = scored else {
            unreachable!()
        };
        assert_eq!(*issued, t0());
        // Nowcast perfect; persistence (the echo at pixel 0) misses
        // entirely. The 55-minute analysis would have scored 1.0.
        assert_eq!(csi[0], (Some(1.0), Some(0.0)));
        assert_eq!(ring.stats()[1].csi_permille[0], Some((1000, 0)));
    }

    /// One forecast in flight per lead: later generations do not replace
    /// it, a verified slot refills from the verifying generation, and the
    /// bytes held are the frames in flight with a shared persistence frame
    /// counted once.
    #[test]
    fn the_ring_holds_only_the_forecasts_in_flight() {
        let frame_bytes = quiet().heap_bytes();
        let mut ring = SkillRing::<Grid, u8>::new();
        assert_eq!(ring.heap_bytes(), 0);
        feed(&mut ring, 1, &generation(0, 120, quiet(), &[]));
        let both_from_zero = vec![(30, t0(), minutes(30)), (60, t0(), minutes(60))];
        assert_eq!(ring.pending(), both_from_zero);
        // Two forecasts and the one analysis both were issued from.
        assert_eq!(ring.heap_bytes(), 3 * frame_bytes);
        for anchor in (5..30).step_by(5) {
            feed(&mut ring, 1, &generation(anchor, 120, quiet(), &[]));
            assert_eq!(ring.pending(), both_from_zero, "at +{anchor}");
            assert_eq!(ring.heap_bytes(), 3 * frame_bytes);
        }
        feed(&mut ring, 1, &generation(30, 120, quiet(), &[]));
        assert_eq!(
            ring.pending(),
            vec![(30, minutes(30), minutes(60)), (60, t0(), minutes(60))]
        );
        // Issued from different analyses now: four frames.
        assert_eq!(ring.heap_bytes(), 4 * frame_bytes);
        for anchor in (35..=60).step_by(5) {
            feed(&mut ring, 1, &generation(anchor, 120, quiet(), &[]));
        }
        assert_eq!(
            ring.pending(),
            vec![
                (30, minutes(60), minutes(90)),
                (60, minutes(60), minutes(120))
            ]
        );
        assert_eq!(ring.heap_bytes(), 3 * frame_bytes);
        let [lead30, lead60] = ring.stats();
        assert_eq!((lead30.scored_pairs, lead60.scored_pairs), (2, 1));
    }

    /// A valid time that passes without its analysis, and a grid change,
    /// each drop the pair and count it; the slot refills at once.
    #[test]
    fn skipped_pairs_are_counted_by_reason() {
        let mut ring = SkillRing::<Grid, u8>::new();
        feed(&mut ring, 1, &generation(0, 60, quiet(), &[]));
        // The source skips the 30-minute frame.
        let outcomes = feed(&mut ring, 1, &generation(35, 60, quiet(), &[]));
        assert_eq!(
            outcomes,
            vec![Outcome::Skipped {
                lead_minutes: 30,
                valid: minutes(30),
                reason: SkipReason::NoAnalysis,
            }]
        );
        assert_eq!(
            ring.pending(),
            vec![(30, minutes(35), minutes(65)), (60, t0(), minutes(60))]
        );
        // The grid changes before either valid time: neither can verify.
        let outcomes = feed(&mut ring, 2, &generation(40, 60, quiet(), &[]));
        assert_eq!(
            outcomes,
            vec![
                Outcome::Skipped {
                    lead_minutes: 30,
                    valid: minutes(65),
                    reason: SkipReason::GridMismatch,
                },
                Outcome::Skipped {
                    lead_minutes: 60,
                    valid: minutes(60),
                    reason: SkipReason::GridMismatch,
                },
            ]
        );
        assert_eq!(
            ring.pending(),
            vec![
                (30, minutes(40), minutes(70)),
                (60, minutes(40), minutes(100))
            ]
        );
        let [lead30, lead60] = ring.stats();
        assert_eq!(
            (lead30.skipped_no_analysis, lead30.skipped_grid_mismatch),
            (1, 1)
        );
        assert_eq!(
            (lead60.skipped_no_analysis, lead60.skipped_grid_mismatch),
            (0, 1)
        );
        assert_eq!((lead30.scored_pairs, lead60.scored_pairs), (0, 0));
        assert_eq!(lead30.csi_permille, [None, None]);
    }

    /// A pair with no echo at a threshold clears that threshold's gauge
    /// pair instead of leaving the previous score up as current.
    #[test]
    fn a_pair_without_events_clears_the_last_score() {
        let wet = row([40.0; W]);
        let rain = row([25.0; W]);
        let mut ring = SkillRing::<Grid, u8>::new();
        feed(&mut ring, 1, &generation(0, 30, wet.clone(), &[]));
        feed(&mut ring, 1, &generation(30, 30, wet.clone(), &[]));
        assert_eq!(
            ring.stats()[0].csi_permille,
            [Some((1000, 1000)), Some((1000, 1000))]
        );
        feed(&mut ring, 1, &generation(60, 30, rain, &[]));
        // Rain everywhere: the issued 40 dBZ frames false-alarm at 35, so
        // both CSIs are 0 there, a measurement.
        assert_eq!(
            ring.stats()[0].csi_permille,
            [Some((1000, 1000)), Some((0, 0))]
        );
        feed(&mut ring, 1, &generation(90, 30, quiet(), &[]));
        // Issued from 25 dBZ: nothing reaches 35 on any side.
        assert_eq!(ring.stats()[0].csi_permille, [Some((0, 0)), None]);
        assert_eq!(ring.stats()[0].scored_pairs, 3);
    }

    /// A lead the forecast schedule never reaches is reported once, not
    /// every generation, and again only after it was issuable in between.
    #[test]
    fn an_unreachable_lead_is_reported_once() {
        let mut ring = SkillRing::<Grid, u8>::new();
        let outcomes = feed(&mut ring, 1, &generation(0, 30, quiet(), &[]));
        assert_eq!(outcomes, vec![Outcome::Unissuable { lead_minutes: 60 }]);
        assert_eq!(ring.pending(), vec![(30, t0(), minutes(30))]);
        assert!(feed(&mut ring, 1, &generation(5, 30, quiet(), &[])).is_empty());
        feed(&mut ring, 1, &generation(10, 60, quiet(), &[]));
        let outcomes = feed(&mut ring, 1, &generation(70, 30, quiet(), &[]));
        assert!(
            outcomes.contains(&Outcome::Unissuable { lead_minutes: 60 }),
            "{outcomes:?}"
        );
    }
}
