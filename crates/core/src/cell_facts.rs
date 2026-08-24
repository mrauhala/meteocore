//! Storm-cell fact sheets — the fused, structured description of one tracked
//! cell that every downstream consumer reads.
//!
//! This is deliberately the *widest* domain type in the cell pipeline, and it
//! has four consumers by design:
//!
//! 1. **Feature properties** — what OGC API - Features serves today.
//! 2. **Significance scoring** — via [`SignificanceTerms`], so ranking reads
//!    the same facts clients see rather than a private parallel structure.
//! 3. **Narrative rendering** — a template today, an LLM prompt later. Because
//!    the prompt is built from this struct alone, every number in a generated
//!    narrative is traceable to a field here.
//! 4. **Learned models** — the feature row a gradient-boosted hazard model
//!    consumes (#541 V2.4). Building the row once and serving all four is the
//!    reason this type exists instead of four ad-hoc projections.
//!
//! Optional groups (`lightning`, `volume`, `impact`, `environment`) follow the
//! same tri-state discipline as the lightning feature properties: `None` means
//! "no source wired or the join was skipped", never "measured zero". Scoring
//! renormalizes around absent groups, so wiring a new source later changes
//! rankings without needing a config flag day.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::significance::{SignificanceScore, SignificanceTerms, Term};

/// TRT-lite severity rank from 2-D attributes (documented heuristic v1).
///
/// Lives in ds-core rather than in the nowcast engine so the ranking, the
/// narrative and the engine cannot drift to different meanings of "severe".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Weak,
    Moderate,
    Severe,
    VerySevere,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Weak => "weak",
            Severity::Moderate => "moderate",
            Severity::Severe => "severe",
            Severity::VerySevere => "very_severe",
        }
    }

    /// 0..=3, for scoring and for ordering.
    pub fn rank(&self) -> u8 {
        match self {
            Severity::Weak => 0,
            Severity::Moderate => 1,
            Severity::Severe => 2,
            Severity::VerySevere => 3,
        }
    }
}

/// Volume-proxy lifecycle vs the previous observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    Growing,
    Decaying,
}

impl Trend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Trend::Growing => "growing",
            Trend::Decaying => "decaying",
        }
    }
}

/// Per-cell lightning attribution (#549). `None` on the fact sheet means no
/// event source is wired or the join was skipped this generation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LightningFacts {
    pub flash_count: u32,
    pub flash_rate_per_min: f64,
    /// Flashes per km² of cell footprint.
    ///
    /// Normalizes for size: a small intense cell and a large diffuse one can
    /// share a flash count while meaning quite different things.
    pub flash_density_per_km2: f64,
    /// Schultz-style 2σ jump fired this generation. Derived from `jump_sigma`
    /// so the two cannot disagree.
    pub jump: bool,
    /// How far above its own recent baseline this cell's flash rate sits, in
    /// standard deviations.
    ///
    /// `None` until there is enough history to have a baseline — not 0.0,
    /// which would claim "measured, no anomaly". A 4σ surge and a 2.1σ nudge
    /// are different facts that `jump` alone cannot distinguish.
    pub jump_sigma: Option<f64>,
    /// Cloud-to-ground and intra-cloud counts, when the source reports the
    /// discriminator. `None` = not reported, never a defaulted zero.
    pub cg_count: Option<u32>,
    pub ic_count: Option<u32>,
    /// How many CG flashes had their polarity reported — the DENOMINATOR
    /// behind `positive_cg_fraction`, and the sample size behind it.
    ///
    /// Exposed rather than kept private for the same reason `beam_coverage`
    /// is: a share whose denominator is invisible invites confident statements
    /// it cannot support. "3 of 4 positive" and "300 of 400 positive" are the
    /// same fraction and not the same evidence.
    pub cg_polarity_known: Option<u32>,
    /// Share of POLARITY-KNOWN cloud-to-ground flashes that were positive,
    /// 0..=1.
    ///
    /// `None` when polarity is not reported **or when no CG flash was
    /// classifiable** — 0 of 0 is not 0%, and reporting it as 0% would invite
    /// "no positive strikes" about a cell with no strikes to classify.
    pub positive_cg_fraction: Option<f64>,
    /// When this track was first attributed a flash.
    ///
    /// Electrification age: a cell producing its first flash now is a
    /// different situation from one active for an hour.
    pub first_flash: Option<DateTime<Utc>>,
}

/// Attributes derived from the 3-D polar volume, joined onto a 2-D track.
///
/// `beam_coverage` is the honesty field and it is not optional: a cell at
/// long range has its lowest surveyed beam kilometres above ground, so
/// `vil_kg_m2`, `base_m` and `volume_km3` are systematically biased. Reporting
/// them without their sampling quality — or letting them rank a cell highly —
/// launders a fabrication through a real number.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct VolumeFacts {
    pub vil_kg_m2: f64,
    pub echo_top_m: f64,
    pub base_m: f64,
    pub volume_km3: f64,
    /// Distance from the contributing radar to the cell centroid.
    pub range_km: f64,
    /// How well the volume was actually sampled, 0.0 (unusable) ..= 1.0
    /// (fully surveyed to the surface).
    pub beam_coverage: f64,
    /// How many per-site cells were aggregated into this record — a composite
    /// mosaic cell can span several. 1 = clean one-to-one.
    pub contributing_cells: u32,
}

/// Where the cell is and what it is heading toward.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImpactFacts {
    /// Named area currently under the cell, if any.
    pub over: Option<String>,
    /// Named area the cell reaches next along its motion vector.
    pub approaching: Option<String>,
    /// Minutes until it reaches `approaching`.
    pub eta_minutes: Option<f64>,
    /// Pre-normalized 0..=1 exposure, so the scorer stays domain-agnostic
    /// about how "how much does this matter to people" was computed.
    pub exposure: f64,
}

/// One sampled NWP environment value at the cell centroid.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnvironmentFact {
    pub name: String,
    pub value: f64,
    pub unit: String,
}

/// Everything known about one tracked cell at one instant.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CellFactSheet {
    /// Stable track id, monotonic across generations.
    pub id: u64,
    /// The analysis instant this describes.
    pub observed: DateTime<Utc>,
    pub lon: f64,
    pub lat: f64,
    pub severity: Severity,
    pub max_dbz: f64,
    pub area_km2: f64,
    /// Generations observed; 1 = newborn.
    pub age: u32,
    pub speed_ms: Option<f64>,
    /// Compass bearing the cell moves toward.
    pub bearing_deg: Option<f64>,
    pub deviant_mover: bool,
    /// Persistent, near-stationary echo — most likely ground clutter (wind
    /// turbines, masts) rather than weather. See [`is_likely_clutter`].
    pub likely_clutter: bool,
    pub trend: Option<Trend>,
    /// Measured intensity tendency, dBZ per minute.
    pub intensity_trend_dbz_min: Option<f64>,
    pub lightning: Option<LightningFacts>,
    pub volume: Option<VolumeFacts>,
    pub impact: Option<ImpactFacts>,
    /// Empty when no environment source is wired.
    pub environment: Vec<EnvironmentFact>,
}

/// Upper end of each scoring term's useful range. Values at or above these
/// saturate at 1.0 — the difference between a 65 and a 70 dBZ core is not
/// what should decide a ranking.
const DBZ_FLOOR: f64 = 35.0;
const DBZ_CEILING: f64 = 60.0;
const AREA_CEILING_KM2: f64 = 200.0;
const FLASH_RATE_CEILING_PER_MIN: f64 = 60.0;
const VIL_CEILING_KG_M2: f64 = 50.0;
const ECHO_TOP_CEILING_M: f64 = 15_000.0;
const INTENSITY_TREND_CEILING_DBZ_MIN: f64 = 2.0;
/// A jump only counts from the 2σ test threshold up; 6σ saturates, since the
/// difference between a 6σ and a 9σ surge is not what should decide a rank.
const JUMP_SIGMA_FLOOR: f64 = 2.0;
const JUMP_SIGMA_CEILING: f64 = 6.0;
/// Positive-CG share ramp. Roughly 10-20% of CG flashes are positive in
/// ordinary storms, so a floor at 5% keeps normal background from scoring at
/// all; a CG population half positive is already the anomalous severe-storm
/// signature, so the term saturates there rather than reserving its top half
/// for fractions that essentially never occur.
const POSITIVE_CG_FLOOR: f64 = 0.05;
const POSITIVE_CG_CEILING: f64 = 0.5;

/// Speed below which an echo is "not going anywhere" (m/s).
///
/// Finnish convection typically tracks 5–20 m/s. Observed clutter sat at 0.1
/// and 2.4 m/s while every real cell in the same frame ran 8.6–12.9 m/s on a
/// single coherent bearing, so the populations separate cleanly here. Matches
/// `DEVIANT_MIN_CELL_SPEED_MS` in cells2d, which already treats motion below
/// this as too small to reason about.
const CLUTTER_MAX_SPEED_MS: f64 = 3.0;

/// Frames a cell must have been stationary for before it is called clutter.
///
/// Persistence is what separates clutter from weather, not slowness alone: a
/// genuine cell can crawl for a few minutes in weak flow, but one that has
/// held both position AND high reflectivity for half an hour is a fixed
/// object. Six frames ≈ 30 min at the 5-minute cadence.
const CLUTTER_MIN_AGE: u32 = 6;

/// Whether a cell looks like ground clutter rather than weather.
///
/// **Mitigation, not detection.** Wind turbine clutter is a genuinely hard
/// upstream QC problem; this only stops a fixed echo dominating a ranking on
/// a quiet day. It will miss clutter that happens to sit under moving weather
/// and could in principle flag a truly stalled storm — which is why the
/// result is surfaced as a fact and demoted, never dropped.
///
/// A newborn track has no velocity yet. `None` means "not known", so it is
/// never treated as stationary — the opposite reading would flag every cell
/// for the first frames after a reload.
pub fn is_likely_clutter(speed_ms: Option<f64>, age: u32) -> bool {
    match speed_ms {
        Some(speed) => speed < CLUTTER_MAX_SPEED_MS && age >= CLUTTER_MIN_AGE,
        None => false,
    }
}

/// Default significance weights for storm cells.
///
/// `impact` is deliberately the largest. A moderate cell reaching a populated
/// area in fifteen minutes matters more than a very severe cell drifting over
/// open sea, and any ranker that sorts on reflectivity alone is wrong in
/// exactly the way that matters operationally.
///
/// These are a defensible starting point, not a calibrated model — they are
/// the baseline a learned ranker has to beat on the object-verification
/// harness before it replaces them.
pub const DEFAULT_CELL_WEIGHTS: &[(&str, f64)] = &[
    ("severity", 1.0),
    ("max_dbz", 0.6),
    ("area", 0.3),
    ("trend", 0.5),
    ("deviant_mover", 0.4),
    ("lightning_jump", 0.9),
    ("flash_rate", 0.5),
    // A high positive-CG share is a well-established severe-storm signal,
    // independent of how MUCH lightning there is.
    ("positive_cg", 0.6),
    ("vil", 0.7),
    ("echo_top", 0.5),
    ("beam_coverage", 0.4),
    ("impact", 1.5),
    // Negative, and as large as the biggest positive term: a fixed echo
    // maximizes severity, max_dbz and impact at once (it is bright, compact
    // and usually over a town), so anything smaller leaves it near the top.
    ("clutter", -1.5),
];

/// Map `value` onto 0..=1 across `floor..=ceiling`, saturating at both ends.
fn ramp(value: f64, floor: f64, ceiling: f64) -> f64 {
    if !value.is_finite() || ceiling <= floor {
        return 0.0;
    }
    ((value - floor) / (ceiling - floor)).clamp(0.0, 1.0)
}

impl SignificanceTerms for CellFactSheet {
    fn terms(&self) -> Vec<Term> {
        let mut terms = vec![
            Term::new("severity", f64::from(self.severity.rank()) / 3.0),
            Term::new("max_dbz", ramp(self.max_dbz, DBZ_FLOOR, DBZ_CEILING)),
            Term::new("area", ramp(self.area_km2, 0.0, AREA_CEILING_KM2)),
            Term::flag("deviant_mover", self.deviant_mover),
            Term::flag("clutter", self.likely_clutter),
        ];

        // Prefer the measured tendency over the coarse growing/decaying flag;
        // fall back to the flag, and emit nothing for a newborn track (no
        // trend exists yet, so it should not be scored as "not growing").
        if let Some(trend) = self.intensity_trend_dbz_min {
            terms.push(Term::new(
                "trend",
                ramp(
                    trend,
                    -INTENSITY_TREND_CEILING_DBZ_MIN,
                    INTENSITY_TREND_CEILING_DBZ_MIN,
                ),
            ));
        } else if let Some(trend) = self.trend {
            terms.push(Term::flag("trend", trend == Trend::Growing));
        }

        if let Some(lightning) = self.lightning {
            // Scaled by magnitude rather than a flag: a 5σ surge should
            // outrank a cell that merely crossed the threshold. Falls back to
            // the boolean when there is no baseline yet.
            terms.push(Term::new(
                "lightning_jump",
                match lightning.jump_sigma {
                    Some(sigma) if lightning.jump => {
                        ramp(sigma, JUMP_SIGMA_FLOOR, JUMP_SIGMA_CEILING)
                    }
                    _ if lightning.jump => 1.0,
                    _ => 0.0,
                },
            ));
            if let Some(frac) = lightning.positive_cg_fraction {
                // Ramped like every other term: the raw fraction would let an
                // ordinary 10% background share carry real weight while a 50%
                // share — already the severe signature — scored only half.
                terms.push(Term::new(
                    "positive_cg",
                    ramp(frac, POSITIVE_CG_FLOOR, POSITIVE_CG_CEILING),
                ));
            }
            terms.push(Term::new(
                "flash_rate",
                ramp(
                    lightning.flash_rate_per_min,
                    0.0,
                    FLASH_RATE_CEILING_PER_MIN,
                ),
            ));
        }

        if let Some(volume) = self.volume {
            // Scale the volume-derived terms by how well the volume was
            // actually sampled, so a far-range cell cannot ride an inflated
            // VIL to the top of the list.
            let coverage = volume.beam_coverage.clamp(0.0, 1.0);
            terms.push(Term::new(
                "vil",
                ramp(volume.vil_kg_m2, 0.0, VIL_CEILING_KG_M2) * coverage,
            ));
            terms.push(Term::new(
                "echo_top",
                ramp(volume.echo_top_m, 0.0, ECHO_TOP_CEILING_M) * coverage,
            ));
            terms.push(Term::new("beam_coverage", coverage));
        }

        if let Some(impact) = &self.impact {
            terms.push(Term::new("impact", impact.exposure));
        }

        terms
    }
}

/// A fact sheet with its computed score. Kept as a pair rather than a field on
/// [`CellFactSheet`] so that scoring reads facts and never its own output.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredCell {
    pub facts: CellFactSheet,
    pub significance: SignificanceScore,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::significance::WeightedScorer;
    use chrono::TimeZone;

    fn scorer() -> WeightedScorer {
        WeightedScorer::new(DEFAULT_CELL_WEIGHTS)
    }

    fn cell(id: u64) -> CellFactSheet {
        CellFactSheet {
            id,
            observed: Utc.with_ymd_and_hms(2026, 8, 19, 12, 0, 0).unwrap(),
            lon: 24.94,
            lat: 60.17,
            severity: Severity::Moderate,
            max_dbz: 47.0,
            area_km2: 40.0,
            age: 5,
            speed_ms: Some(14.0),
            bearing_deg: Some(45.0),
            deviant_mover: false,
            likely_clutter: false,
            trend: None,
            intensity_trend_dbz_min: None,
            lightning: None,
            volume: None,
            impact: None,
            environment: Vec::new(),
        }
    }

    #[test]
    fn default_weights_cover_every_term_the_facts_emit() {
        // A term with no weight is silently ignored by the scorer, so a typo
        // or a newly added term would vanish without this check.
        let mut full = cell(1);
        full.trend = Some(Trend::Growing);
        full.intensity_trend_dbz_min = Some(0.4);
        full.lightning = Some(LightningFacts {
            cg_count: Some(8),
            ic_count: Some(4),
            cg_polarity_known: None,
            positive_cg_fraction: Some(0.25),
            flash_count: 12,
            flash_rate_per_min: 4.0,
            flash_density_per_km2: 0.0,
            jump_sigma: None,
            first_flash: None,
            jump: true,
        });
        full.volume = Some(VolumeFacts {
            vil_kg_m2: 20.0,
            echo_top_m: 9000.0,
            base_m: 500.0,
            volume_km3: 30.0,
            range_km: 60.0,
            beam_coverage: 0.9,
            contributing_cells: 1,
        });
        full.impact = Some(ImpactFacts {
            over: None,
            approaching: Some("Hyvinkää".into()),
            eta_minutes: Some(18.0),
            exposure: 0.6,
        });

        let weighted: Vec<&str> = DEFAULT_CELL_WEIGHTS.iter().map(|(n, _)| *n).collect();
        for term in full.terms() {
            assert!(
                weighted.contains(&term.name),
                "term '{}' has no default weight",
                term.name
            );
        }
        // And the reverse: no weight names a term that is never emitted.
        let emitted: Vec<&str> = full.terms().iter().map(|t| t.name).collect();
        for name in &weighted {
            assert!(emitted.contains(name), "weight '{name}' matches no term");
        }
    }

    #[test]
    fn newborn_track_emits_no_trend_term() {
        // A track with no measured trend must not be scored as "not growing".
        let newborn = cell(1);
        assert!(!newborn.terms().iter().any(|t| t.name == "trend"));

        let mut aged = cell(2);
        aged.trend = Some(Trend::Decaying);
        assert!(aged.terms().iter().any(|t| t.name == "trend"));
    }

    #[test]
    fn absent_groups_renormalize_rather_than_penalize() {
        // Wiring a lightning source must not make quiet cells score lower
        // than they did when no source existed.
        let bare = cell(1);
        let mut quiet = cell(1);
        quiet.lightning = Some(LightningFacts {
            flash_count: 0,
            flash_rate_per_min: 0.0,
            flash_density_per_km2: 0.0,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_polarity_known: None,
            positive_cg_fraction: None,
            first_flash: None,
            jump: false,
        });
        let s = scorer();
        let bare_score = s.score_one(&bare).score;
        let quiet_score = s.score_one(&quiet).score;
        assert!(
            quiet_score < bare_score,
            "measured-quiet is real information and should rank below unknown"
        );
        assert!(quiet_score > 0.0);
    }

    #[test]
    fn poor_beam_coverage_demotes_an_otherwise_identical_cell() {
        let volume = VolumeFacts {
            vil_kg_m2: 40.0,
            echo_top_m: 12_000.0,
            base_m: 300.0,
            volume_km3: 80.0,
            range_km: 40.0,
            beam_coverage: 1.0,
            contributing_cells: 1,
        };
        let mut near = cell(1);
        near.volume = Some(volume);
        let mut far = cell(2);
        far.volume = Some(VolumeFacts {
            range_km: 210.0,
            beam_coverage: 0.2,
            ..volume
        });

        let s = scorer();
        assert!(
            s.score_one(&far).score < s.score_one(&near).score,
            "a far-range cell with identical raw attributes must rank lower"
        );
    }

    #[test]
    fn impact_outranks_raw_intensity() {
        // The operational case: a moderate cell closing on a town beats a
        // very severe cell over open water.
        let mut over_sea = cell(1);
        over_sea.severity = Severity::VerySevere;
        over_sea.max_dbz = 58.0;
        over_sea.area_km2 = 120.0;
        over_sea.impact = Some(ImpactFacts {
            over: None,
            approaching: None,
            eta_minutes: None,
            exposure: 0.0,
        });

        let mut closing = cell(2);
        closing.severity = Severity::Moderate;
        closing.max_dbz = 47.0;
        closing.impact = Some(ImpactFacts {
            over: None,
            approaching: Some("Hyvinkää".into()),
            eta_minutes: Some(15.0),
            exposure: 1.0,
        });

        let s = scorer();
        assert!(
            s.score_one(&closing).score > s.score_one(&over_sea).score,
            "impact must dominate raw intensity"
        );
    }

    #[test]
    fn lightning_jump_lifts_a_cell() {
        let mut quiet = cell(1);
        quiet.lightning = Some(LightningFacts {
            flash_count: 2,
            flash_rate_per_min: 1.0,
            flash_density_per_km2: 0.0,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_polarity_known: None,
            positive_cg_fraction: None,
            first_flash: None,
            jump: false,
        });
        let mut jumping = cell(2);
        jumping.lightning = Some(LightningFacts {
            flash_count: 2,
            flash_rate_per_min: 1.0,
            flash_density_per_km2: 0.0,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_polarity_known: None,
            positive_cg_fraction: None,
            first_flash: None,
            jump: true,
        });
        let s = scorer();
        assert!(s.score_one(&jumping).score > s.score_one(&quiet).score);
    }

    #[test]
    fn ranking_a_realistic_set_puts_the_dangerous_cell_first() {
        let mut ordinary = cell(1);
        ordinary.severity = Severity::Moderate;

        let mut dangerous = cell(2);
        dangerous.severity = Severity::VerySevere;
        dangerous.max_dbz = 57.0;
        dangerous.area_km2 = 90.0;
        dangerous.deviant_mover = true;
        dangerous.intensity_trend_dbz_min = Some(1.2);
        dangerous.lightning = Some(LightningFacts {
            flash_count: 40,
            flash_rate_per_min: 25.0,
            flash_density_per_km2: 0.0,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_polarity_known: None,
            positive_cg_fraction: None,
            first_flash: None,
            jump: true,
        });
        dangerous.impact = Some(ImpactFacts {
            over: Some("Nurmijärvi".into()),
            approaching: Some("Hyvinkää".into()),
            eta_minutes: Some(12.0),
            exposure: 0.85,
        });

        let mut weak = cell(3);
        weak.severity = Severity::Weak;
        weak.max_dbz = 36.0;
        weak.area_km2 = 5.0;

        let scores = scorer().rank(&[ordinary, dangerous, weak]);
        assert_eq!(scores[1].rank, 1, "the dangerous cell must rank first");
        assert_eq!(scores[0].rank, 2);
        assert_eq!(scores[2].rank, 3);
        assert_eq!(
            scores[1].contributions[0].term, "impact",
            "the top reason should be the one a forecaster would lead with"
        );
    }

    #[test]
    fn ramp_saturates_and_survives_garbage() {
        assert_eq!(ramp(10.0, 0.0, 100.0), 0.1);
        assert_eq!(ramp(-5.0, 0.0, 100.0), 0.0);
        assert_eq!(ramp(500.0, 0.0, 100.0), 1.0);
        assert_eq!(ramp(f64::NAN, 0.0, 100.0), 0.0);
        assert_eq!(ramp(1.0, 5.0, 5.0), 0.0);
    }

    #[test]
    fn a_persistent_stationary_echo_is_flagged_as_clutter() {
        // The reported case: wind turbine clutter near Oulu outranked real
        // weather on a quiet day. Observed 0.1 and 2.4 m/s while every real
        // cell in the same frame ran 8.6-12.9 m/s.
        assert!(is_likely_clutter(Some(0.1), 10));
        assert!(is_likely_clutter(Some(2.4), 6));
    }

    #[test]
    fn clutter_needs_both_stationary_and_persistent() {
        // Slow but young: a cell can crawl briefly in weak flow.
        assert!(!is_likely_clutter(Some(0.1), 2));
        // Persistent but moving: an ordinary long-lived storm.
        assert!(!is_likely_clutter(Some(9.7), 20));
    }

    #[test]
    fn a_newborn_track_is_never_clutter() {
        // speed is None until the second observation. Reading that as
        // "stationary" would flag every cell for the first frames after a
        // reload, when every track is new.
        assert!(!is_likely_clutter(None, 1));
        assert!(!is_likely_clutter(None, 50));
    }

    #[test]
    fn flagging_clutter_sinks_it_below_real_weather() {
        // A bright, compact, well-placed fixed echo maximizes severity,
        // max_dbz and impact at once — the demotion has to overcome all of
        // them together.
        let mut clutter = cell(1);
        clutter.severity = Severity::Severe;
        clutter.max_dbz = 54.5;
        clutter.likely_clutter = true;
        clutter.impact = Some(ImpactFacts {
            over: Some("Oulu".into()),
            approaching: None,
            eta_minutes: None,
            exposure: 0.9,
        });

        let mut weather = cell(2);
        weather.severity = Severity::Moderate;
        weather.max_dbz = 47.5;
        weather.impact = Some(ImpactFacts {
            over: Some("Tampere".into()),
            approaching: None,
            eta_minutes: None,
            exposure: 0.7,
        });

        let scores = scorer().rank(&[clutter, weather]);
        assert_eq!(
            scores[1].rank, 1,
            "real weather must outrank a brighter fixed echo"
        );
        assert!(
            scores[0].significance_is_demoted(),
            "and the clutter term should be the reason: {:?}",
            scores[0].contributions
        );
    }

    #[test]
    fn jump_magnitude_outranks_a_bare_threshold_crossing() {
        // The point of keeping sigma: a 5σ surge and a 2.1σ nudge both set
        // `jump`, but they are not the same fact.
        let mut small = cell(1);
        small.lightning = Some(LightningFacts {
            flash_count: 30,
            flash_rate_per_min: 12.0,
            flash_density_per_km2: 0.5,
            jump: true,
            jump_sigma: Some(2.1),
            cg_count: None,
            ic_count: None,
            cg_polarity_known: None,
            positive_cg_fraction: None,
            first_flash: None,
        });
        let mut big = cell(2);
        big.lightning = Some(LightningFacts {
            jump_sigma: Some(5.5),
            ..small.lightning.unwrap()
        });
        let s = scorer();
        assert!(
            s.score_one(&big).score > s.score_one(&small).score,
            "a larger jump must score higher"
        );
    }

    #[test]
    fn a_jump_without_a_baseline_still_counts() {
        // sigma is None until there is history. The cell still jumped; it
        // just cannot be graded, so it must not score as if it had not.
        let mut c = cell(1);
        c.lightning = Some(LightningFacts {
            flash_count: 40,
            flash_rate_per_min: 20.0,
            flash_density_per_km2: 1.0,
            jump: true,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_polarity_known: None,
            positive_cg_fraction: None,
            first_flash: None,
        });
        let mut quiet = c.clone();
        quiet.lightning = Some(LightningFacts {
            jump: false,
            ..c.lightning.unwrap()
        });
        let s = scorer();
        assert!(s.score_one(&c).score > s.score_one(&quiet).score);
    }

    #[test]
    fn a_high_positive_cg_share_raises_significance() {
        let base = LightningFacts {
            flash_count: 20,
            flash_rate_per_min: 8.0,
            flash_density_per_km2: 0.5,
            jump: false,
            jump_sigma: Some(0.5),
            cg_count: Some(20),
            ic_count: Some(0),
            cg_polarity_known: None,
            positive_cg_fraction: Some(0.05),
            first_flash: None,
        };
        let mut ordinary = cell(1);
        ordinary.lightning = Some(base);
        let mut anomalous = cell(2);
        anomalous.lightning = Some(LightningFacts {
            cg_polarity_known: None,
            positive_cg_fraction: Some(0.75),
            ..base
        });
        let s = scorer();
        assert!(
            s.score_one(&anomalous).score > s.score_one(&ordinary).score,
            "a positive-CG-dominated cell is the severe signal"
        );
    }

    #[test]
    fn an_unreported_polarity_share_is_not_scored_as_zero() {
        // None must drop the term (renormalizing), not contribute 0.0 —
        // otherwise wiring a network that omits polarity would silently
        // penalize every cell.
        let mut unknown = cell(1);
        unknown.lightning = Some(LightningFacts {
            flash_count: 20,
            flash_rate_per_min: 8.0,
            flash_density_per_km2: 0.5,
            jump: false,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_polarity_known: None,
            positive_cg_fraction: None,
            first_flash: None,
        });
        assert!(
            !unknown.terms().iter().any(|t| t.name == "positive_cg"),
            "an unreported share must emit no term at all"
        );
    }

    #[test]
    fn positive_cg_saturates_at_the_documented_ceiling() {
        // Pins the RAMP, which relative ordering alone cannot: a raw fraction
        // and a ramped one both put 0.75 above 0.05.
        let facts = |frac: f64| {
            let mut c = cell(1);
            c.lightning = Some(LightningFacts {
                flash_count: 20,
                flash_rate_per_min: 8.0,
                flash_density_per_km2: 0.5,
                jump: false,
                jump_sigma: None,
                cg_count: Some(20),
                ic_count: Some(0),
                cg_polarity_known: None,
                positive_cg_fraction: Some(frac),
                first_flash: None,
            });
            c
        };
        let term = |c: &CellFactSheet| {
            c.terms()
                .iter()
                .find(|t| t.name == "positive_cg")
                .expect("term present")
                .value
        };
        // Background share sits at the floor and contributes nothing.
        assert_eq!(term(&facts(0.05)), 0.0);
        assert_eq!(term(&facts(0.02)), 0.0);
        // A half-positive CG population is already maximal, and anything
        // beyond it stays there rather than needing 100% to saturate.
        assert_eq!(term(&facts(0.5)), 1.0);
        assert_eq!(term(&facts(0.9)), 1.0);
        // Midpoint of the ramp, not of 0..1.
        assert!((term(&facts(0.275)) - 0.5).abs() < 1e-9);
    }
}
