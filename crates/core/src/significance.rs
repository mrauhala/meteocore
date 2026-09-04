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
}

impl Term {
    pub fn new(name: &'static str, value: f64) -> Self {
        Self { name, value }
    }

    /// A boolean signal as a term: present or not.
    pub fn flag(name: &'static str, set: bool) -> Self {
        Self {
            name,
            value: if set { 1.0 } else { 0.0 },
        }
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
    /// Unclamped weighted mean. `contributions` sum to THIS, not to `score`;
    /// they differ only when discount terms push the total below zero.
    pub raw: f64,
    /// 1-based, within the scored set. Ties break by input order.
    pub rank: usize,
    /// Sorted by descending absolute value — the biggest reasons first,
    /// which is also the order a narrative should mention them in.
    pub contributions: Vec<Contribution>,
}

impl SignificanceScore {
    /// Whether any term pulled this score DOWN — i.e. a discount actually
    /// applied, rather than the object merely scoring low.
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

        for term in &terms {
            let Some(weight) = self.weights.get(term.name).copied() else {
                continue;
            };
            if weight == 0.0 {
                continue;
            }
            denominator += weight.abs();
            contributions.push(Contribution {
                term: term.name,
                value: weight * term.normalized(),
            });
        }

        if denominator == 0.0 {
            return SignificanceScore::zero(0);
        }

        for contribution in &mut contributions {
            contribution.value /= denominator;
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

        let raw: f64 = contributions.iter().map(|c| c.value).sum();
        SignificanceScore {
            score: raw.clamp(0.0, 1.0),
            raw,
            rank: 0,
            contributions,
        }
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
        let mut scores: Vec<SignificanceScore> =
            items.iter().map(|item| self.score_one(item)).collect();

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
