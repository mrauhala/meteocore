//! EDR 1.2 `datetime` lists (`datetime=T1,T2,T3`, `/req/core/datetime-response`
//! D): one engine query per instant, merged into one response.
//!
//! Engines take a single `(start, end)` window and each has its own rule for
//! an instant `(t, t)` (exact match, latest-not-after, the scan it would
//! render, …). Querying every listed instant as that window keeps the rule
//! identical to a request naming the instant alone; merging then puts the
//! answers back together:
//!
//! - coverages with a leading `t` axis (`PointSeries`, a `Grid` with `t`) that
//!   agree on everything but time — same position, grid, `z` and parameters —
//!   become one coverage whose `t` axis holds every instant's steps, ascending,
//!   each step once (two instants an engine snaps to the same step yield it
//!   once);
//! - any other coverage (a `VerticalProfile`, a `Section`, …) is kept as it
//!   is, and an identical one from another instant is dropped.
//!
//! The response is `Single` when every instant answered `Single` and the merge
//! left one coverage, else a `CoverageCollection`.

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::model::{CoverageResponse, DomainDescription, NdArray, QueryResult, VerticalCoord};

use crate::params::DatetimeSelector;

/// Value budget of one datetime-list response across all its instants: the
/// budget a single gridded area answer has (`MAX_AREA_VALUES`), and the same
/// as the position budget, so a list cannot multiply what one request may
/// return.
pub const MAX_LIST_VALUES: usize = ds_core::feature::MAX_AREA_VALUES;

type Window = (DateTime<Utc>, DateTime<Utc>);

/// Run `query` for a parsed `datetime`: once with the window (or `None`), or
/// once per listed instant via [`query_instants`].
pub fn run(
    datetime: Option<&DatetimeSelector>,
    expired: impl Fn() -> bool,
    mut query: impl FnMut(Option<Window>) -> Result<CoverageResponse, DataServerError>,
) -> Result<CoverageResponse, DataServerError> {
    match datetime {
        None => query(None),
        Some(DatetimeSelector::Window(start, end)) => query(Some((*start, *end))),
        Some(DatetimeSelector::Instants(instants)) => {
            query_instants(instants, expired, |w| query(Some(w)))
        }
    }
}

/// Query each instant as the window `(t, t)` and merge the answers.
///
/// An instant with no data — the engine's `LocationNotFound`, the 404 a
/// single instant gets — contributes nothing; when no instant has data the
/// first instant's error is the answer. Any other error fails the request,
/// as it would for that instant alone. The merged response is bounded by
/// [`MAX_LIST_VALUES`], and `expired` is checked before every instant.
pub fn query_instants(
    instants: &[DateTime<Utc>],
    expired: impl Fn() -> bool,
    mut query: impl FnMut(Window) -> Result<CoverageResponse, DataServerError>,
) -> Result<CoverageResponse, DataServerError> {
    let mut responses = Vec::with_capacity(instants.len());
    let mut no_data = None;
    let mut values = 0usize;
    for &t in instants {
        if expired() {
            return Err(DataServerError::DeadlineExceeded);
        }
        match query((t, t)) {
            Ok(response) => {
                values = values.saturating_add(count_values(&response));
                if values > MAX_LIST_VALUES {
                    return Err(DataServerError::QueryTooLarge(format!(
                        "the datetime list's responses exceed {MAX_LIST_VALUES} values — \
                         name fewer instants, parameters or a smaller area"
                    )));
                }
                responses.push(response);
            }
            Err(e @ DataServerError::LocationNotFound(_)) => {
                no_data.get_or_insert(e);
            }
            Err(e) => return Err(e),
        }
    }
    if responses.is_empty() {
        return Err(no_data.unwrap_or_else(|| {
            DataServerError::LocationNotFound("no data at any listed datetime".into())
        }));
    }
    Ok(merge(responses))
}

fn count_values(response: &CoverageResponse) -> usize {
    let coverages: &[QueryResult] = match response {
        CoverageResponse::Single(q) => std::slice::from_ref(q),
        CoverageResponse::Collection(v) => v,
    };
    coverages
        .iter()
        .flat_map(|q| q.ranges.values())
        .map(|r| r.values.len())
        .fold(0usize, usize::saturating_add)
}

/// Merge per-instant responses, given in instant order (see the module docs).
pub fn merge(responses: Vec<CoverageResponse>) -> CoverageResponse {
    let all_single = responses
        .iter()
        .all(|r| matches!(r, CoverageResponse::Single(_)));
    let mut merged: Vec<QueryResult> = Vec::new();
    for response in responses {
        let coverages = match response {
            CoverageResponse::Single(q) => vec![q],
            CoverageResponse::Collection(v) => v,
        };
        for coverage in coverages {
            add(&mut merged, coverage);
        }
    }
    for coverage in &mut merged {
        sort_steps(coverage);
    }
    if all_single && merged.len() == 1 {
        CoverageResponse::Single(merged.remove(0))
    } else {
        CoverageResponse::Collection(merged)
    }
}

fn add(merged: &mut Vec<QueryResult>, coverage: QueryResult) {
    if time_axis(&coverage).is_some() {
        if let Some(series) = merged.iter_mut().find(|m| same_series(m, &coverage)) {
            append_steps(series, &coverage);
            return;
        }
    } else if merged
        .iter()
        .any(|m| same_domain(&m.domain, &coverage.domain) && same_parameters(m, &coverage))
    {
        return;
    }
    merged.push(coverage);
}

/// The coverage's time axis when every range leads with it, so steps can be
/// appended along it; `None` for any other coverage.
fn time_axis(q: &QueryResult) -> Option<&[DateTime<Utc>]> {
    let t = match &q.domain {
        DomainDescription::PointSeries { t, .. } | DomainDescription::Grid { t: Some(t), .. } => t,
        _ => return None,
    };
    q.ranges
        .values()
        .all(|r| {
            r.axis_names.first().map(String::as_str) == Some("t")
                && r.shape.first() == Some(&t.len())
                && r.shape.iter().product::<usize>() == r.values.len()
        })
        .then_some(t.as_slice())
}

fn time_axis_mut(domain: &mut DomainDescription) -> Option<&mut Vec<DateTime<Utc>>> {
    match domain {
        DomainDescription::PointSeries { t, .. } | DomainDescription::Grid { t: Some(t), .. } => {
            Some(t)
        }
        _ => None,
    }
}

fn same_z(a: &Option<VerticalCoord>, b: &Option<VerticalCoord>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.kind == b.kind && a.values == b.values,
        _ => false,
    }
}

fn same_parameters(a: &QueryResult, b: &QueryResult) -> bool {
    a.parameters.len() == b.parameters.len()
        && a.parameters.keys().all(|k| b.parameters.contains_key(k))
        && a.ranges.len() == b.ranges.len()
        && a.ranges.keys().all(|k| b.ranges.contains_key(k))
}

/// Two time-axis coverages that differ in nothing but their steps.
fn same_series(a: &QueryResult, b: &QueryResult) -> bool {
    if time_axis(a).is_none() || time_axis(b).is_none() {
        return false;
    }
    let same_place = match (&a.domain, &b.domain) {
        (
            DomainDescription::PointSeries {
                x: ax,
                y: ay,
                z: az,
                ..
            },
            DomainDescription::PointSeries {
                x: bx,
                y: by,
                z: bz,
                ..
            },
        ) => ax == bx && ay == by && same_z(az, bz),
        (
            DomainDescription::Grid {
                x: ax,
                y: ay,
                z: az,
                ..
            },
            DomainDescription::Grid {
                x: bx,
                y: by,
                z: bz,
                ..
            },
        ) => ax == bx && ay == by && same_z(az, bz),
        _ => false,
    };
    same_place
        && same_parameters(a, b)
        && a.ranges.iter().all(|(k, ra)| {
            b.ranges
                .get(k)
                .is_some_and(|rb| ra.axis_names == rb.axis_names && ra.shape[1..] == rb.shape[1..])
        })
}

/// Identical domains, for coverages that cannot be merged along time.
fn same_domain(a: &DomainDescription, b: &DomainDescription) -> bool {
    use DomainDescription as D;
    match (a, b) {
        (
            D::Point {
                x: ax,
                y: ay,
                t: at,
                z: az,
            },
            D::Point {
                x: bx,
                y: by,
                t: bt,
                z: bz,
            },
        ) => ax == bx && ay == by && at == bt && same_z(az, bz),
        (
            D::PointSeries {
                x: ax,
                y: ay,
                t: at,
                z: az,
            },
            D::PointSeries {
                x: bx,
                y: by,
                t: bt,
                z: bz,
            },
        ) => ax == bx && ay == by && at == bt && same_z(az, bz),
        (
            D::Grid {
                x: ax,
                y: ay,
                t: at,
                z: az,
            },
            D::Grid {
                x: bx,
                y: by,
                t: bt,
                z: bz,
            },
        ) => ax == bx && ay == by && at == bt && same_z(az, bz),
        (
            D::VerticalProfile {
                x: ax,
                y: ay,
                t: at,
                z: az,
            },
            D::VerticalProfile {
                x: bx,
                y: by,
                t: bt,
                z: bz,
            },
        ) => ax == bx && ay == by && at == bt && az.kind == bz.kind && az.values == bz.values,
        (
            D::Section {
                nodes: an,
                z: az,
                coverage_floor: af,
            },
            D::Section {
                nodes: bn,
                z: bz,
                coverage_floor: bf,
            },
        ) => an == bn && az.kind == bz.kind && az.values == bz.values && af == bf,
        _ => false,
    }
}

/// Append `from`'s steps that `series` does not hold yet.
fn append_steps(series: &mut QueryResult, from: &QueryResult) {
    let Some(from_t) = time_axis(from) else {
        return;
    };
    let QueryResult { domain, ranges, .. } = series;
    let Some(t) = time_axis_mut(domain) else {
        return;
    };
    for (i, step) in from_t.iter().enumerate() {
        if t.contains(step) {
            continue;
        }
        t.push(*step);
        for (name, range) in ranges.iter_mut() {
            let source = &from.ranges[name];
            let stride = stride(source);
            range
                .values
                .extend_from_slice(&source.values[i * stride..(i + 1) * stride]);
            range.shape[0] += 1;
        }
    }
}

/// Values per step of a range that leads with `t`.
fn stride(range: &NdArray) -> usize {
    range.shape[1..].iter().product()
}

/// Order a merged series' steps by time, carrying the range values along.
fn sort_steps(coverage: &mut QueryResult) {
    let QueryResult { domain, ranges, .. } = coverage;
    let Some(t) = time_axis_mut(domain) else {
        return;
    };
    if t.is_sorted() {
        return;
    }
    let mut order: Vec<usize> = (0..t.len()).collect();
    order.sort_by_key(|&i| t[i]);
    *t = order.iter().map(|&i| t[i]).collect();
    for range in ranges.values_mut() {
        let stride = stride(range);
        range.values = order
            .iter()
            .flat_map(|&i| range.values[i * stride..(i + 1) * stride].iter().copied())
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::model::ParameterDescription;
    use ds_core::vertical::VerticalKind;
    use std::collections::HashMap;

    fn at(hour: u32) -> DateTime<Utc> {
        format!("2026-06-01T{hour:02}:00:00Z").parse().unwrap()
    }

    fn param() -> HashMap<String, ParameterDescription> {
        HashMap::from([(
            "temperature".to_string(),
            ParameterDescription {
                label: "temperature".into(),
                unit: "K".into(),
                observed_property: "temperature".into(),
                standard_name: None,
            },
        )])
    }

    fn series(x: f64, steps: &[(u32, f64)]) -> QueryResult {
        QueryResult {
            domain: DomainDescription::PointSeries {
                x,
                y: 60.0,
                t: steps.iter().map(|&(h, _)| at(h)).collect(),
                z: None,
            },
            parameters: param(),
            ranges: HashMap::from([(
                "temperature".to_string(),
                NdArray {
                    shape: vec![steps.len()],
                    axis_names: vec!["t".into()],
                    values: steps.iter().map(|&(_, v)| Some(v)).collect(),
                },
            )]),
        }
    }

    /// A 1-step `[t, y, x]` grid of 2 × 2 cells, every cell `v`.
    fn grid(hour: u32, v: f64) -> QueryResult {
        QueryResult {
            domain: DomainDescription::Grid {
                x: vec![24.0, 25.0],
                y: vec![60.0, 61.0],
                t: Some(vec![at(hour)]),
                z: None,
            },
            parameters: param(),
            ranges: HashMap::from([(
                "temperature".to_string(),
                NdArray {
                    shape: vec![1, 2, 2],
                    axis_names: vec!["t".into(), "y".into(), "x".into()],
                    values: vec![Some(v); 4],
                },
            )]),
        }
    }

    fn profile(hour: u32) -> QueryResult {
        QueryResult {
            domain: DomainDescription::VerticalProfile {
                x: 24.0,
                y: 60.0,
                t: Some(at(hour)),
                z: VerticalCoord {
                    kind: VerticalKind::Pressure,
                    values: vec![850.0, 500.0],
                },
            },
            parameters: param(),
            ranges: HashMap::from([(
                "temperature".to_string(),
                NdArray {
                    shape: vec![2],
                    axis_names: vec!["z".into()],
                    values: vec![Some(270.0), Some(250.0)],
                },
            )]),
        }
    }

    fn times(q: &QueryResult) -> Vec<DateTime<Utc>> {
        time_axis(q).unwrap().to_vec()
    }

    fn values(q: &QueryResult) -> Vec<Option<f64>> {
        q.ranges["temperature"].values.clone()
    }

    #[test]
    fn point_series_merge_into_one_ascending_series() {
        // Out of order on purpose: the merge sorts steps by time.
        let merged = merge(vec![
            CoverageResponse::Single(series(24.0, &[(3, 3.0)])),
            CoverageResponse::Single(series(24.0, &[(1, 1.0)])),
            CoverageResponse::Single(series(24.0, &[(2, 2.0)])),
        ]);
        let CoverageResponse::Single(q) = merged else {
            panic!("three single series of one point merge into one coverage");
        };
        assert_eq!(times(&q), vec![at(1), at(2), at(3)]);
        assert_eq!(values(&q), vec![Some(1.0), Some(2.0), Some(3.0)]);
        assert_eq!(q.ranges["temperature"].shape, vec![3]);
    }

    #[test]
    fn a_step_two_instants_snap_to_appears_once() {
        let merged = merge(vec![
            CoverageResponse::Single(series(24.0, &[(1, 1.0)])),
            CoverageResponse::Single(series(24.0, &[(1, 1.0)])),
        ]);
        let CoverageResponse::Single(q) = merged else {
            panic!("expected one coverage");
        };
        assert_eq!(times(&q), vec![at(1)]);
        assert_eq!(q.ranges["temperature"].shape, vec![1]);
    }

    #[test]
    fn grids_merge_along_t_keeping_cells_per_step() {
        let merged = merge(vec![
            CoverageResponse::Single(grid(2, 2.0)),
            CoverageResponse::Single(grid(1, 1.0)),
        ]);
        let CoverageResponse::Single(q) = merged else {
            panic!("expected one grid");
        };
        assert_eq!(times(&q), vec![at(1), at(2)]);
        assert_eq!(q.ranges["temperature"].shape, vec![2, 2, 2]);
        let mut expected = vec![Some(1.0); 4];
        expected.extend(vec![Some(2.0); 4]);
        assert_eq!(values(&q), expected);
    }

    #[test]
    fn stations_merge_per_station_in_a_collection() {
        let merged = merge(vec![
            CoverageResponse::Collection(vec![
                series(24.0, &[(1, 1.0)]),
                series(25.0, &[(1, 5.0)]),
            ]),
            CoverageResponse::Collection(vec![series(25.0, &[(2, 6.0)])]),
        ]);
        let CoverageResponse::Collection(v) = merged else {
            panic!("a collection stays a collection");
        };
        assert_eq!(v.len(), 2);
        assert_eq!(times(&v[0]), vec![at(1)]);
        assert_eq!(times(&v[1]), vec![at(1), at(2)]);
        assert_eq!(values(&v[1]), vec![Some(5.0), Some(6.0)]);
    }

    #[test]
    fn profiles_stay_separate_and_duplicates_drop() {
        let merged = merge(vec![
            CoverageResponse::Single(profile(1)),
            CoverageResponse::Single(profile(2)),
            CoverageResponse::Single(profile(2)),
        ]);
        let CoverageResponse::Collection(v) = merged else {
            panic!("profiles at two times cannot share one coverage");
        };
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn different_parameters_do_not_merge() {
        let mut other = series(24.0, &[(2, 2.0)]);
        let range = other.ranges.remove("temperature").unwrap();
        other.ranges.insert("humidity".into(), range);
        let description = other.parameters.remove("temperature").unwrap();
        other.parameters.insert("humidity".into(), description);
        let merged = merge(vec![
            CoverageResponse::Single(series(24.0, &[(1, 1.0)])),
            CoverageResponse::Single(other),
        ]);
        assert!(matches!(merged, CoverageResponse::Collection(v) if v.len() == 2));
    }

    #[test]
    fn instants_without_data_are_skipped() {
        let got = query_instants(
            &[at(1), at(2), at(3)],
            || false,
            |(start, end)| {
                assert_eq!(start, end, "every instant is queried as (t, t)");
                if start == at(2) {
                    Err(DataServerError::LocationNotFound("no data".into()))
                } else {
                    Ok(CoverageResponse::Single(series(
                        24.0,
                        &[(start.format("%H").to_string().parse().unwrap(), 1.0)],
                    )))
                }
            },
        )
        .unwrap();
        let CoverageResponse::Single(q) = got else {
            panic!("expected one series");
        };
        assert_eq!(times(&q), vec![at(1), at(3)]);
    }

    #[test]
    fn no_data_at_any_instant_is_the_first_not_found() {
        let err = query_instants(
            &[at(1), at(2)],
            || false,
            |(t, _)| Err(DataServerError::LocationNotFound(t.to_rfc3339())),
        )
        .unwrap_err();
        assert!(matches!(err, DataServerError::LocationNotFound(m) if m == at(1).to_rfc3339()));
    }

    #[test]
    fn other_errors_fail_the_list() {
        let err = query_instants(
            &[at(1), at(2)],
            || false,
            |(t, _)| {
                if t == at(2) {
                    Err(DataServerError::InvalidParameter("bad".into()))
                } else {
                    Ok(CoverageResponse::Single(series(24.0, &[(1, 1.0)])))
                }
            },
        )
        .unwrap_err();
        assert!(matches!(err, DataServerError::InvalidParameter(_)));
    }

    #[test]
    fn expired_budget_stops_before_the_next_instant() {
        let mut calls = 0;
        let err = query_instants(
            &[at(1), at(2)],
            || true,
            |_| {
                calls += 1;
                Ok(CoverageResponse::Single(series(24.0, &[(1, 1.0)])))
            },
        )
        .unwrap_err();
        assert!(matches!(err, DataServerError::DeadlineExceeded));
        assert_eq!(calls, 0);
    }

    #[test]
    fn combined_values_are_bounded() {
        let big = || {
            let mut q = series(24.0, &[(1, 1.0)]);
            let range = q.ranges.get_mut("temperature").unwrap();
            range.values = vec![None; MAX_LIST_VALUES / 2 + 1];
            CoverageResponse::Single(q)
        };
        let err = query_instants(&[at(1), at(2)], || false, |_| Ok(big())).unwrap_err();
        assert!(matches!(err, DataServerError::QueryTooLarge(_)));
    }
}
