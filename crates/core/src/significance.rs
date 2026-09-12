//! Significance scoring and ranking — "which of these objects matter?".
//!
//! On a convective day the nowcast tracker carries ~150 storm cells. Every
//! consumer (top-K narration, alert prioritisation, MCP tool responses, map
//! label decluttering) needs the same answer to the same question, so the
//! answer lives in one place rather than in a `sort_by` at each call site.
//!
//! Framework-free like the rest of ds-core. Deliberately domain-agnostic:
//! the scorer sees normalized [`Term`]s, never a storm cell — the same
//! machinery ranks CAP alerts by urgency or impact events by priority.
//!
//! The design contract worth preserving:
//!
//! - **Scores are explainable.** [`SignificanceScore::contributions`] carries
//!   the per-term breakdown, so an operator can ask *why* an object ranked
//!   third and get an arguable answer. A bare scalar cannot be reviewed, and
//!   a ranking nobody can review is a ranking nobody should trust.
//! - **Absent terms renormalize.** A term the domain could not compute this
//!   cycle (a 3-D attribute before the volume join runs, an impact term with
//!   no geometry source wired) drops out of both numerator and denominator.
//!   The same weight table therefore works before and after such a source is
//!   added — no flag day.
//! - **Weights may be negative.** A data-quality term is a *discount*: a
//!   storm cell at 200 km range has its lowest surveyed beam near 3 km, so
//!   its derived volume attributes are systematically biased and it should
//!   rank BELOW an equally intense cell observed well. Scoring machinery that
//!   can only add would promote far-range artifacts.
//! - **[`WeightedScorer`] is the baseline, not the ceiling.** A learned model
//!   (gradient-boosted trees over the same feature row) is another
//!   [`Significance`] impl with SHAP-style attributions filling the same
//!   `contributions` field, swappable behind this interface.

use std::collections::BTreeMap;

use crate::error::DataServerError;

/// How a term takes part in the score (#645).
///
/// A GRADED term measures how much of something an object has — intensity,
/// size, exposure. Graded weights form the denominator, so the graded part
/// of the score is a mean of what was measured and absent graded terms
/// renormalize.
///
/// A BONUS term is a signal that either fires or does not — a deviant mover,
/// a lightning jump, a clutter verdict, a trend. Its weight is NOT in the
/// denominator, and it composes so the score stays inside `0..=1` by
/// construction rather than by clamping:
///
/// - a bonus with a POSITIVE weight fills the remaining headroom:
///   `s += (1 − s) · c`, with `c = w·v / denominator`. Several positive
///   bonuses combine as a soft OR, so three signals firing at once cannot
///   push the top cells past 1.0 into a clamped tie whose order is then
///   decided by id;
/// - a bonus with a NEGATIVE weight is a multiplicative DISCOUNT:
///   `s *= 1 − |w| · v`, with `|w|` the fraction removed at full value
///   (`−0.9` keeps a tenth). Not relative to the denominator: a fixed echo
///   must sink whatever else was wired, and an additive `−1.5` over a
///   graded mass of 3.4 only ever removed 0.44 of the score — the live
///   Utajärvi clutter cell stayed at rank 1 that way.
///
/// `contributions` still sum to `raw`: each bonus's contribution is its
/// share of the headroom filled or the score removed. A flag that did not
/// fire contributes nothing and dilutes nothing — the reason the split
/// exists. With every flag in the denominator, a plain cell with no flags
/// set was capped at 0.51 and the weak class packed into a tenth of the
/// range (#636).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    Graded,
    Bonus,
}

/// One normalized input to a score, in `0.0..=1.0`.
///
/// Normalization is the domain's job: it knows that 60 dBZ is the top of the
/// reflectivity scale and that a 3-generation deviant streak is as deviant as
/// it needs to get. Values outside the range are clamped (and NaN maps to 0)
/// rather than rejected — a scoring bug should degrade a ranking, never take
/// down a poll cycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Term {
    /// Stable identifier, matched against the weight table. `&'static str`
    /// on purpose: these become metric labels, so the set must be bounded
    /// at compile time.
    pub name: &'static str,
    pub value: f64,
    pub kind: TermKind,
}

impl Term {
    /// A graded term: in the denominator.
    pub fn new(name: &'static str, value: f64) -> Self {
        Self {
            name,
            value,
            kind: TermKind::Graded,
        }
    }

    /// A bonus term with a magnitude in `0..=1`: outside the denominator.
    pub fn bonus(name: &'static str, value: f64) -> Self {
        Self {
            name,
            value,
            kind: TermKind::Bonus,
        }
    }

    /// A boolean signal as a bonus term: fires or does not.
    pub fn flag(name: &'static str, set: bool) -> Self {
        Self::bonus(name, if set { 1.0 } else { 0.0 })
    }

    /// Clamped to `0.0..=1.0`, NaN → 0.0.
    fn normalized(&self) -> f64 {
        if self.value.is_nan() {
            0.0
        } else {
            self.value.clamp(0.0, 1.0)
        }
    }
}

/// What one term contributed to a score. Sums over all contributions equal
/// [`SignificanceScore::raw`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contribution {
    pub term: &'static str,
    /// Signed: negative for discount terms.
    pub value: f64,
}

/// A scored, ranked object.
#[derive(Debug, Clone, PartialEq)]
pub struct SignificanceScore {
    /// Clamped to `0.0..=1.0` — what clients see and sort on.
    pub score: f64,
    /// Unclamped total. `contributions` sum to THIS, not to `score`; the two
    /// differ only when a NEGATIVE graded weight pushes the graded mean below
    /// zero (bonuses and discounts are bounded by construction, see
    /// [`TermKind`]).
    pub raw: f64,
    /// 1-based, within the scored set. Ties break by input order.
    pub rank: usize,
    /// Sorted by descending absolute value — the biggest reasons first,
    /// which is also the order a narrative should mention them in.
    pub contributions: Vec<Contribution>,
}

impl SignificanceScore {
    /// Whether any term pulled this score DOWN — a discount actually applied
    /// (clutter, weakening, a negative graded weight), rather than the object
    /// merely scoring low.
    pub fn significance_is_demoted(&self) -> bool {
        self.contributions.iter().any(|c| c.value < 0.0)
    }

    /// The score an object gets when nothing about it could be evaluated.
    pub fn zero(rank: usize) -> Self {
        Self {
            score: 0.0,
            raw: 0.0,
            rank,
            contributions: Vec::new(),
        }
    }
}

/// Anything that can be scored.
pub trait SignificanceTerms {
    /// The terms available for THIS object. Returning fewer terms than a
    /// sibling object is expected and handled: absent terms renormalize.
    fn terms(&self) -> Vec<Term>;
}

/// Scores objects by a weighted mean over their normalized terms.
///
/// Built from a domain-supplied default table, optionally overridden by
/// operator config. An override naming a term the domain does not produce is
/// a hard error, not a silent no-op — the same stance the codebase takes on
/// an unknown colormap name, and for the same reason: a typo in a weight key
/// would otherwise quietly leave the default in place and produce a ranking
/// the operator never asked for.
#[derive(Debug, Clone)]
pub struct WeightedScorer {
    weights: BTreeMap<&'static str, f64>,
}

impl WeightedScorer {
    /// Build from the domain's default weights.
    pub fn new(defaults: &[(&'static str, f64)]) -> Self {
        Self {
            weights: defaults.iter().copied().collect(),
        }
    }

    /// Apply operator overrides, keyed by term name.
    ///
    /// Errors on an unknown key (listing the valid ones) or a non-finite
    /// weight. Both are config mistakes worth failing a collection over.
    pub fn with_overrides(
        mut self,
        overrides: &BTreeMap<String, f64>,
    ) -> Result<Self, DataServerError> {
        for (key, value) in overrides {
            if !self.weights.contains_key(key.as_str()) {
                // BTreeMap keys are already sorted — the list reads as a menu.
                let valid: Vec<&str> = self.weights.keys().copied().collect();
                return Err(DataServerError::Config(format!(
                    "unknown significance weight '{key}' (valid: {})",
                    valid.join(", ")
                )));
            }
            if !value.is_finite() {
                return Err(DataServerError::Config(format!(
                    "significance weight '{key}' must be finite, got {value}"
                )));
            }
            if let Some(slot) = self.weights.get_mut(key.as_str()) {
                *slot = *value;
            }
        }
        Ok(self)
    }

    /// The effective weight table, for logging and diagnostics.
    pub fn weights(&self) -> &BTreeMap<&'static str, f64> {
        &self.weights
    }

    /// Score one object without ranking it (`rank` is 0).
    ///
    /// Terms whose name is absent from the weight table are IGNORED — the
    /// weight table is authoritative about what counts. Terms present with a
    /// zero weight are likewise dropped from the denominator, so zeroing a
    /// weight fully disables that term rather than diluting the others.
    pub fn score_one<T: SignificanceTerms + ?Sized>(&self, item: &T) -> SignificanceScore {
        let terms = item.terms();
        let mut contributions = Vec::with_capacity(terms.len());
        let mut denominator = 0.0f64;
        // Bonuses are composed AFTER the graded mean is known (they scale
        // with its headroom), so collect them first: (index into
        // `contributions`, weight, normalized value).
        let mut bonuses: Vec<(usize, f64, f64)> = Vec::new();

        for term in &terms {
            let Some(weight) = self.weights.get(term.name).copied() else {
                continue;
            };
            if weight == 0.0 {
                continue;
            }
            match term.kind {
                TermKind::Graded => {
                    denominator += weight.abs();
                    contributions.push(Contribution {
                        term: term.name,
                        value: weight * term.normalized(),
                    });
                }
                TermKind::Bonus => {
                    bonuses.push((contributions.len(), weight, term.normalized()));
                    contributions.push(Contribution {
                        term: term.name,
                        value: 0.0,
                    });
                }
            }
        }

        // Nothing measured ⇒ nothing to rank, whatever flags fired: a bonus
        // is relative to a mean that does not exist.
        if denominator == 0.0 {
            return SignificanceScore::zero(0);
        }

        let mut graded_mean = 0.0f64;
        for (i, contribution) in contributions.iter_mut().enumerate() {
            if bonuses.iter().all(|&(bi, _, _)| bi != i) {
                contribution.value /= denominator;
                graded_mean += contribution.value;
            }
        }

        // Positive bonuses fill the headroom above the graded mean as a soft
        // OR; each takes a share of the fill proportional to its own pull.
        let base = graded_mean.clamp(0.0, 1.0);
        let pulls: Vec<(usize, f64)> = bonuses
            .iter()
            .filter(|&&(_, w, _)| w > 0.0)
            .map(|&(i, w, v)| (i, (w * v / denominator).clamp(0.0, 1.0)))
            .collect();
        let pull_total: f64 = pulls.iter().map(|&(_, c)| c).sum();
        let mut score = base;
        if pull_total > 0.0 {
            let kept: f64 = pulls.iter().map(|&(_, c)| 1.0 - c).product();
            let filled = (1.0 - base) * (1.0 - kept);
            for &(i, c) in &pulls {
                contributions[i].value = filled * c / pull_total;
            }
            score += filled;
        }

        // Discounts remove a fraction of what is left; each takes a share of
        // the removal proportional to its own cut.
        let cuts: Vec<(usize, f64)> = bonuses
            .iter()
            .filter(|&&(_, w, _)| w < 0.0)
            .map(|&(i, w, v)| (i, (w.abs() * v).clamp(0.0, 1.0)))
            .collect();
        let cut_total: f64 = cuts.iter().map(|&(_, d)| d).sum();
        if cut_total > 0.0 {
            let kept: f64 = cuts.iter().map(|&(_, d)| 1.0 - d).product();
            let removed = score * (1.0 - kept);
            for &(i, d) in &cuts {
                contributions[i].value = -removed * d / cut_total;
            }
            score -= removed;
        }

        // Biggest reasons first — the order a narrative should lead with.
        // Total order (abs desc, then name) so equal-magnitude terms don't
        // reorder between runs and churn ETags.
        contributions.sort_by(|a, b| {
            b.value
                .abs()
                .partial_cmp(&a.value.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.term.cmp(b.term))
        });

        // `raw` = graded mean (possibly below zero under a negative graded
        // weight) plus the bounded bonus and discount parts, so it still
        // equals the contribution sum.
        let raw = graded_mean + (score - base);
        SignificanceScore {
            score: raw.clamp(0.0, 1.0),
            raw,
            rank: 0,
            contributions,
        }
    }

    /// The configured weight of `name`, if any.
    pub fn weight(&self, name: &str) -> Option<f64> {
        self.weights.get(name).copied()
    }

    /// Score and rank a set, returning scores PARALLEL TO THE INPUT (not
    /// reordered) with `rank` filled in.
    ///
    /// Ranking is by descending `score`; ties break by input position, so
    /// callers get deterministic output by supplying a deterministic input
    /// order (e.g. sorted by track id). Without that, a `HashMap` iteration
    /// upstream would reshuffle equal-scoring objects between cycles and
    /// churn every downstream ETag.
    pub fn rank<T: SignificanceTerms>(&self, items: &[T]) -> Vec<SignificanceScore> {
        self.rank_inner(items, None)
    }

    /// [`Self::rank`] with `score` rounded to `decimals` BEFORE ranking, so
    /// the number a client sorts on is the number the rank was computed from.
    ///
    /// Ranking on the raw score and serving it rounded is a residual of the
    /// #635 hole (#644): two cells whose raw scores differ by less than half
    /// a unit in the last served decimal get distinct ranks in raw order but
    /// tie on the wire, and the wire tie-break (input position) can point the
    /// other way — a limited page then holds rank 2 without rank 1.
    /// Rounding first makes the two comparators see one number. `raw` and
    /// `contributions` stay unrounded.
    pub fn rank_quantized<T: SignificanceTerms>(
        &self,
        items: &[T],
        decimals: i32,
    ) -> Vec<SignificanceScore> {
        self.rank_inner(items, Some(decimals))
    }

    fn rank_inner<T: SignificanceTerms>(
        &self,
        items: &[T],
        decimals: Option<i32>,
    ) -> Vec<SignificanceScore> {
        let mut scores: Vec<SignificanceScore> =
            items.iter().map(|item| self.score_one(item)).collect();
        if let Some(d) = decimals {
            let f = 10f64.powi(d);
            for s in &mut scores {
                s.score = (s.score * f).round() / f;
            }
        }

        // Ties break by INPUT POSITION, so the caller decides the tie-break by
        // choosing the input order. That is load-bearing: whatever serves the
        // ranked objects must sort ties the same way, or the rank a client
        // reads and the order it receives disagree.
        //
        // Observed 2026-09-04 (#635): two cells shared significance 0.2595,
        // the page returned them in one order and the ranks in the other, and
        // a `limit: 30` page came back holding ranks 1–29 and 31 — a hole
        // where nothing was actually skipped. Ties are common because the
        // score is published to four decimals over a narrow range.
        let mut order: Vec<usize> = (0..scores.len()).collect();
        order.sort_by(|&a, &b| {
            scores[b]
                .score
                .partial_cmp(&scores[a].score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(&b))
        });
        for (position, &index) in order.iter().enumerate() {
            scores[index].rank = position + 1;
        }
        scores
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Item(Vec<Term>);

    impl SignificanceTerms for Item {
        fn terms(&self) -> Vec<Term> {
            self.0.clone()
        }
    }

    fn scorer() -> WeightedScorer {
        WeightedScorer::new(&[("severity", 1.0), ("impact", 1.5), ("beam_quality", -0.6)])
    }

    #[test]
    fn rank_quantized_ranks_on_the_number_the_client_sees() {
        // Two items 1e-6 apart in raw score, the LOWER one first in input
        // order. Raw ranking puts the higher second item first; a client
        // sorting on the 4-dp served value sees a tie and keeps input order.
        // Quantized ranking agrees with the client; raw ranking does not.
        let s = WeightedScorer::new(&[("severity", 1.0)]);
        let items = vec![
            Item(vec![Term::new("severity", 0.500_000)]),
            Item(vec![Term::new("severity", 0.500_001)]),
        ];
        let raw = s.rank(&items);
        assert_eq!(
            (raw[0].rank, raw[1].rank),
            (2, 1),
            "precondition: raw order differs"
        );
        let q = s.rank_quantized(&items, 4);
        assert_eq!(
            (q[0].rank, q[1].rank),
            (1, 2),
            "tie on the wire ⇒ input order"
        );
        assert_eq!(q[0].score, q[1].score, "served scores are one number");
        assert!((q[1].raw - 0.500_001).abs() < 1e-9, "raw stays unrounded");
    }

    #[test]
    fn bonus_terms_stay_out_of_the_denominator() {
        // A flag that did not fire leaves the graded mean untouched; one that
        // fired adds its weight relative to the graded mass (#645).
        let s = WeightedScorer::new(&[("severity", 1.0), ("area", 0.5), ("deviant", 0.3)]);
        let plain = Item(vec![Term::new("severity", 0.5), Term::new("area", 0.5)]);
        let quiet = Item(vec![
            Term::new("severity", 0.5),
            Term::new("area", 0.5),
            Term::flag("deviant", false),
        ]);
        let fired = Item(vec![
            Term::new("severity", 0.5),
            Term::new("area", 0.5),
            Term::flag("deviant", true),
        ]);
        assert_eq!(s.score_one(&plain).score, s.score_one(&quiet).score);
        assert!((s.score_one(&quiet).score - 0.5).abs() < 1e-12);
        // Fills 0.3 / (1.0 + 0.5) = 0.2 of the remaining headroom: 0.5 + 0.5·0.2
        assert!((s.score_one(&fired).score - 0.6).abs() < 1e-12);
        let sum: f64 = s
            .score_one(&fired)
            .contributions
            .iter()
            .map(|c| c.value)
            .sum();
        assert!((sum - s.score_one(&fired).raw).abs() < 1e-12);
    }

    #[test]
    fn bonuses_cannot_push_the_score_past_one() {
        // Three strong bonuses on a strong cell: the old additive form gave
        // raw 1.27 and a clamped tie at the top of the list, ordered by id.
        let s = WeightedScorer::new(&[("severity", 1.0), ("a", 0.9), ("b", 0.9), ("c", 0.9)]);
        let item = Item(vec![
            Term::new("severity", 0.8),
            Term::flag("a", true),
            Term::flag("b", true),
            Term::flag("c", true),
        ]);
        let one = s.score_one(&item);
        assert!(one.score < 1.0 && one.score > 0.99, "got {}", one.score);
        assert!((one.raw - one.score).abs() < 1e-12, "raw is bounded too");
        let sum: f64 = one.contributions.iter().map(|c| c.value).sum();
        assert!((sum - one.raw).abs() < 1e-12);
        // Two such cells with different severities still order by severity.
        let weaker = Item(vec![
            Term::new("severity", 0.7),
            Term::flag("a", true),
            Term::flag("b", true),
            Term::flag("c", true),
        ]);
        assert!(s.score_one(&weaker).score < one.score);
    }

    #[test]
    fn a_discount_removes_its_fraction_whatever_else_is_wired() {
        // −0.9 keeps a tenth, with one graded term or with three: the
        // denominator has no say in how hard a clutter verdict hits.
        let narrow = WeightedScorer::new(&[("severity", 1.0), ("clutter", -0.9)]);
        let wide = WeightedScorer::new(&[
            ("severity", 1.0),
            ("impact", 1.5),
            ("max_dbz", 0.6),
            ("clutter", -0.9),
        ]);
        let n = narrow.score_one(&Item(vec![
            Term::new("severity", 0.8),
            Term::flag("clutter", true),
        ]));
        assert!((n.score - 0.08).abs() < 1e-12, "got {}", n.score);
        let w = wide.score_one(&Item(vec![
            Term::new("severity", 0.8),
            Term::new("impact", 0.8),
            Term::new("max_dbz", 0.8),
            Term::flag("clutter", true),
        ]));
        assert!((w.score - 0.08).abs() < 1e-12, "got {}", w.score);
        assert!(w.significance_is_demoted());
        let sum: f64 = w.contributions.iter().map(|c| c.value).sum();
        assert!((sum - w.raw).abs() < 1e-12);
    }

    #[test]
    fn a_bonus_alone_cannot_score() {
        let s = WeightedScorer::new(&[("deviant", 0.3)]);
        let only_flag = Item(vec![Term::flag("deviant", true)]);
        assert_eq!(s.score_one(&only_flag).score, 0.0);
    }

    #[test]
    fn contributions_sum_to_raw() {
        let item = Item(vec![
            Term::new("severity", 0.75),
            Term::new("impact", 0.5),
            Term::new("beam_quality", 1.0),
        ]);
        let score = scorer().score_one(&item);
        let sum: f64 = score.contributions.iter().map(|c| c.value).sum();
        assert!(
            (sum - score.raw).abs() < 1e-12,
            "contributions {sum} != raw {}",
            score.raw
        );
        // (1.0*0.75 + 1.5*0.5 + -0.6*1.0) / (1.0 + 1.5 + 0.6)
        assert!((score.raw - (0.9 / 3.1)).abs() < 1e-12);
    }

    #[test]
    fn absent_terms_renormalize() {
        // The same object before and after a 3-D join lands: adding a term
        // with weight 0 contribution should not move the others' meaning.
        let without = Item(vec![Term::new("severity", 1.0)]);
        let score = scorer().score_one(&without);
        assert!(
            (score.score - 1.0).abs() < 1e-12,
            "a lone maxed term should score 1.0, got {}",
            score.score
        );
        assert_eq!(score.contributions.len(), 1);
    }

    #[test]
    fn discount_term_demotes() {
        let clean = Item(vec![
            Term::new("severity", 1.0),
            Term::flag("beam_quality", false),
        ]);
        let degraded = Item(vec![
            Term::new("severity", 1.0),
            Term::flag("beam_quality", true),
        ]);
        let s = scorer();
        assert!(
            s.score_one(&degraded).score < s.score_one(&clean).score,
            "a data-quality discount must be able to demote"
        );
    }

    #[test]
    fn score_clamps_but_raw_does_not() {
        // Discounts dominating produces a negative weighted mean.
        let item = Item(vec![Term::new("beam_quality", 1.0)]);
        let score = scorer().score_one(&item);
        assert_eq!(score.score, 0.0);
        assert!(score.raw < 0.0, "raw should stay negative for auditability");
        let sum: f64 = score.contributions.iter().map(|c| c.value).sum();
        assert!((sum - score.raw).abs() < 1e-12);
    }

    #[test]
    fn contributions_are_biggest_first() {
        let item = Item(vec![
            Term::new("severity", 0.2),
            Term::new("impact", 1.0),
            Term::new("beam_quality", 0.5),
        ]);
        let score = scorer().score_one(&item);
        let magnitudes: Vec<f64> = score.contributions.iter().map(|c| c.value.abs()).collect();
        assert!(
            magnitudes.windows(2).all(|w| w[0] >= w[1]),
            "expected descending magnitude, got {magnitudes:?}"
        );
        assert_eq!(score.contributions[0].term, "impact");
    }

    #[test]
    fn unknown_and_zero_weight_terms_are_ignored() {
        let item = Item(vec![
            Term::new("severity", 1.0),
            Term::new("nonexistent", 1.0),
        ]);
        let score = scorer().score_one(&item);
        assert_eq!(score.contributions.len(), 1);

        let zeroed = WeightedScorer::new(&[("severity", 1.0), ("impact", 0.0)]);
        let both = Item(vec![Term::new("severity", 1.0), Term::new("impact", 0.0)]);
        // impact zeroed out entirely rather than dragging the mean to 0.5.
        assert!((zeroed.score_one(&both).score - 1.0).abs() < 1e-12);
    }

    #[test]
    fn no_scorable_terms_yields_zero_not_nan() {
        let score = scorer().score_one(&Item(vec![]));
        assert_eq!(score.score, 0.0);
        assert!(score.raw.is_finite());
        assert!(score.contributions.is_empty());
    }

    #[test]
    fn non_finite_term_values_do_not_poison_the_score() {
        let item = Item(vec![
            Term::new("severity", f64::NAN),
            Term::new("impact", f64::INFINITY),
        ]);
        let score = scorer().score_one(&item);
        assert!(score.score.is_finite(), "NaN/inf must not escape a term");
        // NaN → 0, inf → clamped to 1: 1.5 / 2.5
        assert!((score.raw - 0.6).abs() < 1e-12);
    }

    #[test]
    fn rank_is_parallel_to_input_and_ties_break_by_position() {
        let items = vec![
            Item(vec![Term::new("severity", 0.5)]),
            Item(vec![Term::new("severity", 1.0)]),
            Item(vec![Term::new("severity", 0.5)]),
        ];
        let scores = scorer().rank(&items);
        assert_eq!(scores.len(), 3);
        assert_eq!(scores[1].rank, 1, "highest score ranks first");
        assert_eq!(scores[0].rank, 2, "tie breaks by input position");
        assert_eq!(scores[2].rank, 3);
    }

    #[test]
    fn overrides_apply_and_reject_typos() {
        let overrides: BTreeMap<String, f64> = [("impact".to_string(), 3.0)].into_iter().collect();
        let tuned = scorer().with_overrides(&overrides).expect("valid override");
        assert_eq!(tuned.weights().get("impact"), Some(&3.0));
        assert_eq!(tuned.weights().get("severity"), Some(&1.0));

        let typo: BTreeMap<String, f64> = [("imapct".to_string(), 3.0)].into_iter().collect();
        let err = scorer().with_overrides(&typo).unwrap_err().to_string();
        assert!(
            err.contains("imapct"),
            "error should name the bad key: {err}"
        );
        assert!(
            err.contains("impact"),
            "error should list valid keys: {err}"
        );

        let bad: BTreeMap<String, f64> = [("impact".to_string(), f64::NAN)].into_iter().collect();
        assert!(scorer().with_overrides(&bad).is_err());
    }

    #[test]
    fn ranking_is_stable_across_repeated_runs() {
        let items: Vec<Item> = (0..20)
            .map(|i| Item(vec![Term::new("severity", f64::from(i % 3) / 3.0)]))
            .collect();
        let s = scorer();
        let first = s.rank(&items);
        for _ in 0..5 {
            assert_eq!(s.rank(&items), first, "ranking must be deterministic");
        }
    }
}
