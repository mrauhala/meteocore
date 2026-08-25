//! Clutter-detector regression against an operator-labelled frame (#620).
//!
//! `fixtures/clutter-eval-2026-08-24T2025Z.json` is a verbatim capture of one
//! `get_storm_cells` response, with a ground-truth label supplied by the
//! operator against radar imagery: **every one of its 11 cells is
//! non-meteorological**. There was no real precipitation over Finland at that
//! instant.
//!
//! Why this file exists: clutter-detector changes are otherwise
//! unfalsifiable. This repo has killed ungated work before (#546, growth/decay
//! abandoned after three attempts), and #620 proposes replacing the current
//! heuristic outright. A change that cannot be measured should not be made.
//!
//! **What this fixture can and cannot prove.** It is ALL-NEGATIVE, so it
//! measures RECALL ONLY. A detector that returned `true` unconditionally would
//! score a perfect 11/11 here while being useless. Before this becomes a gate
//! for #620 rather than a baseline, it needs a companion all-positive frame —
//! a real convective day where every flagged cell is a false positive. Do not
//! tune against this file alone.

use serde_json::Value;

use ds_core::cell_facts::is_likely_clutter;

/// Baseline recall of the shipped heuristic on this frame: 2 of 11.
///
/// Not a target. A number to beat, recorded so that "the new detector is
/// better" is a measurement rather than an opinion.
const BASELINE_DETECTIONS: usize = 2;

fn fixture() -> Value {
    let raw = include_str!("fixtures/clutter-eval-2026-08-24T2025Z.json");
    serde_json::from_str(raw).expect("fixture parses")
}

fn cells(doc: &Value) -> Vec<Value> {
    doc["payload"]["cells"]
        .as_array()
        .expect("cells array")
        .clone()
}

#[test]
fn the_fixture_is_the_all_negative_frame_it_claims_to_be() {
    let doc = fixture();
    assert_eq!(
        doc["fixture"]["ground_truth"]["label"], "no_real_precipitation",
        "this harness is only valid for an all-negative frame"
    );
    assert_eq!(cells(&doc).len(), 11);
}

/// Score the CURRENT detector against ground truth, and pin the result.
///
/// Every cell here is clutter, so every `false` is a miss and there are no
/// false positives to trade against — see the module note on why that makes
/// this a baseline rather than a gate.
#[test]
fn the_shipped_detector_recall_is_pinned_at_the_measured_baseline() {
    let doc = fixture();
    let mut detected = Vec::new();
    let mut missed_no_history = Vec::new();
    let mut missed_with_history = Vec::new();

    for c in cells(&doc) {
        let id = c["id"].as_str().unwrap_or_default().to_string();
        let age = c["track_age"].as_u64().unwrap_or(0) as u32;
        let speed = c["speed_ms"].as_f64();
        if is_likely_clutter(speed, age) {
            detected.push(id);
        } else if speed.is_none() {
            missed_no_history.push(id);
        } else {
            missed_with_history.push(id);
        }
    }

    assert_eq!(
        detected.len(),
        BASELINE_DETECTIONS,
        "recall changed: detected {detected:?}. If this is an improvement, \
         raise BASELINE_DETECTIONS deliberately and say so in the commit — \
         the number exists to make that a decision rather than a drift."
    );

    // The two failure classes are structurally different and #620 must
    // address both; a fix that only helps one is a partial fix.
    assert_eq!(
        missed_no_history.len(),
        6,
        "cells with no velocity at all: {missed_no_history:?}"
    );
    assert_eq!(
        missed_with_history.len(),
        3,
        "cells with 2-5 frames, still under the age gate: {missed_with_history:?}"
    );
}

/// The top-ranked cell in this frame was clutter.
///
/// This is the user-visible consequence in one assertion: on a night with no
/// weather at all, the ranking's first answer was a fixed ground echo.
#[test]
fn the_highest_ranked_cell_in_an_all_clutter_frame_was_not_flagged() {
    let doc = fixture();
    let top = cells(&doc)
        .into_iter()
        .find(|c| c["significance_rank"] == 1)
        .expect("a rank-1 cell");
    assert_eq!(top["likely_clutter"], false);
    assert_eq!(
        top["severity"], "severe",
        "and it was labelled severe while being a fixed echo"
    );
}
