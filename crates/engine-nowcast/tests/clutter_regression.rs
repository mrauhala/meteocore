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
//! A second fixture, `clutter-precision-2026-09-04T1245Z.json`, supplies the
//! other side: a 274-cell widespread precipitation event, for measuring FALSE
//! POSITIVES. Its labels are inferred rather than operator-confirmed, so the
//! assertions against it are limited to what net displacement alone
//! establishes — a cell that travelled 18 km is not a fixed ground target,
//! whatever its instantaneous speed.
//!
//! **What the all-negative fixture can and cannot prove.** It is ALL-NEGATIVE, so it
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
        // The fixture predates #631, so it carries no net displacement.
        // `None` is the honest value and must leave the old behaviour intact —
        // that is what makes the baseline still comparable.
        let net = c["net_displacement_km"].as_f64();
        if is_likely_clutter(speed, age, net) {
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

// ---- precision: the widespread-rain frame --------------------------------

/// Net displacement beyond which a cell demonstrably is not a fixed target.
/// Mirrors `CLUTTER_MAX_NET_DISPLACEMENT_KM`; kept local so the test states
/// its own premise rather than inheriting whatever the constant becomes.
const CANNOT_BE_FIXED_KM: f64 = 3.0;

fn precision_frame() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/clutter-precision-2026-09-04T1245Z.json"
    ))
    .expect("fixture parses")
}

/// A cell that has travelled kilometres is never flagged as clutter.
///
/// Before the net-displacement veto, 19 of 21 flagged cells on this frame had
/// moved more than 3 km — up to 18.2 km — and `clutter` LED their
/// `significance_reasons`, demoting severe cells from a median rank of 20 to
/// ranks 135–197. That is real precipitation being suppressed on a rainy day,
/// which is a worse failure than missing clutter on a quiet one.
#[test]
fn no_cell_that_travelled_is_flagged_as_clutter() {
    let doc = precision_frame();
    let cells = doc["payload"]["cells"].as_array().expect("cells");
    assert_eq!(cells.len(), 274, "the captured frame");

    let mut flagged = 0usize;
    let mut travelled_and_flagged = Vec::new();
    for c in cells {
        let age = c["track_age"].as_u64().unwrap_or(0) as u32;
        let speed = c["speed_ms"].as_f64();
        let net = c["net_displacement_km"].as_f64();
        if is_likely_clutter(speed, age, net) {
            flagged += 1;
            if net.is_some_and(|d| d > CANNOT_BE_FIXED_KM) {
                travelled_and_flagged.push((c["id"].clone(), net));
            }
        }
    }

    assert!(
        travelled_and_flagged.is_empty(),
        "cells that moved kilometres were called clutter: {travelled_and_flagged:?}"
    );
    // The two survivors are genuinely stationary (0.5 km and 1.1 km net).
    assert_eq!(
        flagged, 2,
        "expected only the two stationary cells to remain flagged"
    );
}

/// The veto is a veto, not another vote.
///
/// No combination of low speed and long age may outweigh demonstrated travel;
/// and an unknown displacement must not act as a veto either, or every cell
/// predating #631 would escape the detector entirely.
#[test]
fn displacement_overrides_speed_and_age_but_unknown_does_not() {
    // Slow, old, and stationary: still clutter.
    assert!(is_likely_clutter(Some(0.5), 30, Some(0.4)));
    // Equally slow and old, but it went somewhere: not clutter.
    assert!(!is_likely_clutter(Some(0.5), 30, Some(18.2)));
    // Unknown displacement falls back to the speed and age test unchanged.
    assert!(is_likely_clutter(Some(0.5), 30, None));
    assert!(!is_likely_clutter(Some(0.5), 2, None));
}
