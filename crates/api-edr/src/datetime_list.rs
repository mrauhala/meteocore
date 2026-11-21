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
//!   once). A gridded engine answers an instant with a one-step `[t, …]`
//!   grid at the step it served (#1412), so the instants' grids join along
//!   `t` as a cube's do;
//! - any other coverage (a `VerticalProfile`, a `Section`, a `Trajectory`,
//!   a `Grid` without `t`, which only a collection without time answers, …)
//!   is kept as it is. One from another instant is dropped only when it is
//!   identical, its domain, parameters and range values all the same; one
//!   whose values differ stays, and the answer is a `CoverageCollection`.
//!   Nothing here invents a `t` axis: the listed instant is not a step
//!   such a collection holds.
//!
//! The response is `Single` when every instant answered `Single` and the merge
//! left one coverage, else a `CoverageCollection`.
//!
//! The instants are answered side by side (#1522): up to
//! [`INSTANT_CONCURRENCY`] at once, as jobs of
//! `ds_executor::field_jobs::run_field_jobs`, each in an EDR query slot of
//! its own ([`crate::executor::QueryBudget::spare_slots`]). The answers are
//! then taken in the instants' order, by the rules one instant after
//! another followed, so the response, its ETag and its errors are those of
//! answering them in turn.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::feature::AreaShare;
use ds_core::model::{CoverageResponse, DomainDescription, NdArray, QueryResult, VerticalCoord};

use crate::executor::QueryBudget;
use crate::params::DatetimeSelector;

/// Value budget of one datetime-list response across all its instants: the
/// budget a single gridded area answer has (`MAX_AREA_VALUES`), and the same
/// as the position budget, so a list cannot multiply what one request may
/// return.
pub const MAX_LIST_VALUES: usize = ds_core::feature::MAX_AREA_VALUES;

/// The most instants of one list answered at once (#1522). An instant on
/// GeoTIFF or an ODIM composite reads one frame, a round trip on a remote
/// source, so only the list can overlap them: `n` at once cut a 16-instant
/// list's round trips to 16 ÷ `n`. Each instant past the first takes an
/// EDR query slot free at the start, never the last free one, or the list
/// runs them in turn. EDR admits `ds_executor::query_workers()` queries,
/// 2–8, so an idle server answers up to one fewer than that at once, at
/// most 4, and one admitting two answers a list in turn, as a busy one
/// does. So a list answers no more engine queries at once than EDR admits,
/// always leaves a slot for the next query, and each engine's own
/// per-query bounds (GRIB's field memory, an ODIM composite query's one
/// unbudgeted read, NetCDF's read plan) hold per slot, as for separate
/// requests. Four is ds-executor's `FIELD_CONCURRENCY`.
pub const INSTANT_CONCURRENCY: usize = 4;

type Window = (DateTime<Utc>, DateTime<Utc>);

/// The values a list's answers may hold together, and the 400 refusing
/// more.
#[derive(Clone, Copy)]
pub struct ValueBudget {
    pub max: usize,
    pub refuse: fn() -> DataServerError,
}

/// A list's budget: [`MAX_LIST_VALUES`].
pub const LIST_BUDGET: ValueBudget = ValueBudget {
    max: MAX_LIST_VALUES,
    refuse: list_too_large,
};

fn list_too_large() -> DataServerError {
    DataServerError::QueryTooLarge(format!(
        "the datetime list's responses exceed {MAX_LIST_VALUES} values — \
         name fewer instants, parameters or a smaller area"
    ))
}

/// Run `query` for a parsed `datetime`: once with the window (or `None`) on
/// the calling thread, or once per listed instant via [`query_instants`],
/// under [`LIST_BUDGET`].
pub fn run(
    datetime: Option<&DatetimeSelector>,
    budget: &QueryBudget,
    query: impl Fn(Option<Window>) -> Result<CoverageResponse, DataServerError> + Send + Sync + 'static,
) -> Result<CoverageResponse, DataServerError> {
    run_within(datetime, budget, LIST_BUDGET, query)
}

/// [`run`], a list's answers held to `values`: a query type whose own
/// budget refuses a list's answers first (position's, whose 400 a list has
/// always answered).
pub fn run_within(
    datetime: Option<&DatetimeSelector>,
    budget: &QueryBudget,
    values: ValueBudget,
    query: impl Fn(Option<Window>) -> Result<CoverageResponse, DataServerError> + Send + Sync + 'static,
) -> Result<CoverageResponse, DataServerError> {
    match datetime {
        None => query(None),
        Some(DatetimeSelector::Window(start, end)) => query(Some((*start, *end))),
        Some(DatetimeSelector::Instants(instants)) => {
            let wanted = INSTANT_CONCURRENCY.min(instants.len()).saturating_sub(1);
            let spare = budget.spare_slots(wanted);
            query_instants(
                instants,
                || budget.expired(),
                1 + spare.count(),
                values,
                move |window| query(Some(window)),
            )
        }
    }
}

/// What one instant answered.
type Outcome = Result<CoverageResponse, DataServerError>;

/// Query each instant as the window `(t, t)`, `concurrency` at once, and
/// merge the answers.
///
/// An instant with no data — the engine's `LocationNotFound`, the 404 a
/// single instant gets — contributes nothing; when no instant has data the
/// first instant's error is the answer. Any other error fails the request,
/// as it would for that instant alone. The merged response is bounded by
/// `values`, and `expired` is checked before every instant starts.
///
/// The instants start in order and their answers arrive in any order. Once
/// an answer decides the list (an error, or the values that have arrived
/// passing the budget), no instant starts. Those running finish first, so a
/// deciding error waits for them. The answers are then taken in the
/// instants' order, exactly as one after another were: the first instant
/// whose error is not "no data", or whose answer takes the values past the
/// budget, decides the error, and an instant without an answer means the
/// time ran out first, the 504. Here a list past its deadline is that 504
/// only when no earlier instant decided it, but the executor answers a
/// query still running at its deadline with the 504 itself: a deciding
/// error that waits for a sibling running past the deadline reaches the
/// client as the 504. So the outcome never depends on which answer arrived
/// first, and `merge` sees the answers in the instants' order.
///
/// Instants side by side compete for the engines' process-wide fail-fast
/// byte budgets, which one instant at a time never shared with itself. So
/// an instant refused with `ResourceExhausted` while others ran is not an
/// answer: no further instant starts beside it, and once those running
/// finish, the instants without an answer are queried one at a time, in
/// order, until one decides the list. A refusal then is the answer. A
/// refused instant is asked again only when the time left before the
/// deadline covers the time its refusal took, since a store read that
/// failed after its retries is `ResourceExhausted` too; otherwise its
/// refusal is the answer as it is.
///
/// An engine that fits an area grid to the value budget fits each
/// instant's to its share of it ([`AreaShare::answers`], #1436, entered on
/// the thread each instant runs on): every instant then gets the same grid,
/// so the answers join along `t`, and together they fit the list's budget
/// instead of each filling it.
pub fn query_instants<Q>(
    instants: &[DateTime<Utc>],
    expired: impl Fn() -> bool + Sync,
    concurrency: usize,
    values: ValueBudget,
    query: Q,
) -> Result<CoverageResponse, DataServerError>
where
    Q: Fn(Window) -> Result<CoverageResponse, DataServerError> + Send + Sync + 'static,
{
    let answers = instants.len();
    let query = Arc::new(query);
    let job = |query: &Arc<Q>| {
        let query = Arc::clone(query);
        move |(index, t): (usize, DateTime<Utc>)| {
            let _share = AreaShare::answers(answers);
            let started = Instant::now();
            let answer = query((t, t));
            Ok((index, answer, started.elapsed()))
        }
    };
    let mut outcomes: Vec<Option<Outcome>> = Vec::new();
    outcomes.resize_with(answers, || None);
    let outcomes = Mutex::new(outcomes);
    // How long each instant refused side by side took to be refused.
    let mut refused: Vec<Option<Duration>> = vec![None; answers];
    let side_by_side = concurrency > 1;

    // Side by side. Both flags are read and written on the scheduler's
    // thread, between jobs.
    let decided = AtomicBool::new(false);
    let contended = AtomicBool::new(false);
    let mut arrived = 0usize;
    let start = instants.iter().copied().enumerate().take_while(|_| {
        !decided.load(Ordering::Relaxed) && !contended.load(Ordering::Relaxed) && !expired()
    });
    let ran = ds_executor::field_jobs::run_field_jobs(
        ds_executor::query_runtime(),
        concurrency,
        start,
        job(&query),
        |(index, answer, took): (usize, Outcome, Duration)| {
            match &answer {
                Ok(response) => {
                    arrived = arrived.saturating_add(count_values(response));
                    if arrived > values.max {
                        decided.store(true, Ordering::Relaxed);
                    }
                }
                Err(DataServerError::LocationNotFound(_)) => {}
                Err(DataServerError::ResourceExhausted) if side_by_side => {
                    contended.store(true, Ordering::Relaxed);
                    refused[index] = Some(took);
                    return Ok(());
                }
                Err(_) => decided.store(true, Ordering::Relaxed),
            }
            lock(&outcomes)[index] = Some(answer);
            Ok(())
        },
    );
    let on_time = in_time(ran)?;

    // In turn, after a refusal side by side: the instants without an
    // answer, in order, until the answers before the next one decide. A
    // refused instant is asked again only when the time left covers the
    // time its refusal took: `ResourceExhausted` is also a store read that
    // failed after its retries, which another attempt would only carry past
    // the deadline. Without that time its refusal is the answer.
    if on_time && contended.load(Ordering::Relaxed) {
        let deadline = ds_core::deadline::current();
        let time_for = |took: Duration| {
            deadline
                .is_none_or(|deadline| deadline.saturating_duration_since(Instant::now()) >= took)
        };
        let unanswered: Vec<(usize, DateTime<Utc>)> = instants
            .iter()
            .copied()
            .enumerate()
            .filter(|&(index, _)| lock(&outcomes)[index].is_none())
            .collect();
        let next = unanswered
            .into_iter()
            .take_while(|&(index, _)| !decides(&lock(&outcomes)[..index], values.max) && !expired())
            .filter(|&(index, _)| match refused[index] {
                Some(took) if !time_for(took) => {
                    lock(&outcomes)[index] = Some(Err(DataServerError::ResourceExhausted));
                    false
                }
                _ => true,
            });
        let ran = ds_executor::field_jobs::run_field_jobs(
            ds_executor::query_runtime(),
            1,
            next,
            job(&query),
            |(index, answer, _): (usize, Outcome, Duration)| {
                lock(&outcomes)[index] = Some(answer);
                Ok(())
            },
        );
        in_time(ran)?;
    }

    // Every instant started has answered, unless the deadline passed. One
    // without an answer comes after any answer that decided the list, which
    // then decides it here first: else past the deadline, the 504.
    let outcomes = outcomes.into_inner().unwrap_or_else(|e| e.into_inner());
    let mut responses = Vec::with_capacity(answers);
    let mut no_data = None;
    let mut total = 0usize;
    for outcome in outcomes {
        match outcome {
            None => return Err(DataServerError::DeadlineExceeded),
            Some(Ok(response)) => {
                total = total.saturating_add(count_values(&response));
                if total > values.max {
                    return Err((values.refuse)());
                }
                responses.push(response);
            }
            Some(Err(e @ DataServerError::LocationNotFound(_))) => {
                no_data.get_or_insert(e);
            }
            Some(Err(e)) => return Err(e),
        }
    }
    if responses.is_empty() {
        return Err(no_data.unwrap_or_else(|| {
            DataServerError::LocationNotFound("no data at any listed datetime".into())
        }));
    }
    Ok(merge(responses))
}

/// Whether the scheduler ran to the end (`true`) or stopped at the
/// deadline (`false`), whose answers the replay then weighs; any other
/// failure (a job that panicked) is the list's.
fn in_time(ran: Result<(), DataServerError>) -> Result<bool, DataServerError> {
    match ran {
        Ok(()) => Ok(true),
        Err(DataServerError::DeadlineExceeded) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether `outcomes`, the answers of the instants before one, decide the
/// list in order: an error other than "no data", or values past `max`.
fn decides(outcomes: &[Option<Outcome>], max: usize) -> bool {
    let mut total = 0usize;
    for outcome in outcomes {
        match outcome {
            Some(Ok(response)) => {
                total = total.saturating_add(count_values(response));
                if total > max {
                    return true;
                }
            }
            Some(Err(DataServerError::LocationNotFound(_))) => {}
            Some(Err(_)) => return true,
            None => return false,
        }
    }
    false
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
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
    } else if merged.iter().any(|m| {
        same_domain(&m.domain, &coverage.domain)
            && same_parameters(m, &coverage)
            && same_ranges(m, &coverage)
    }) {
        return;
    }
    merged.push(coverage);
}

/// The same ranges: the same names, each with the same shape, axes and
/// values. Without the values, two instants' answers that share a domain
/// but differ in data would keep only the first's.
fn same_ranges(a: &QueryResult, b: &QueryResult) -> bool {
    a.ranges.len() == b.ranges.len()
        && a.ranges.iter().all(|(name, ra)| {
            b.ranges.get(name).is_some_and(|rb| {
                ra.shape == rb.shape && ra.axis_names == rb.axis_names && ra.values == rb.values
            })
        })
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
                coarsened: ac,
                ..
            },
            DomainDescription::Grid {
                x: bx,
                y: by,
                z: bz,
                coarsened: bc,
                ..
            },
        ) => ax == bx && ay == by && same_z(az, bz) && ac == bc,
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
                coarsened: ac,
            },
            D::Grid {
                x: bx,
                y: by,
                t: bt,
                z: bz,
                coarsened: bc,
            },
        ) => ax == bx && ay == by && at == bt && same_z(az, bz) && ac == bc,
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
        (
            D::Trajectory {
                nodes: an,
                node_z: anz,
                z: az,
            },
            D::Trajectory {
                nodes: bn,
                node_z: bnz,
                z: bz,
            },
        ) => an == bn && same_z(anz, bnz) && same_z(az, bz),
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
            // The finer of the two, so no step's values lose digits (#1341).
            range.precision = range.precision.max(source.precision);
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
    use chrono::Timelike;
    use ds_core::model::{ParameterDescription, ValuePrecision};
    use ds_core::vertical::VerticalKind;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// [`query_instants`] as a list runs with every slot it wants free.
    fn listed(
        instants: &[DateTime<Utc>],
        query: impl Fn(Window) -> Result<CoverageResponse, DataServerError> + Send + Sync + 'static,
    ) -> Result<CoverageResponse, DataServerError> {
        query_instants(instants, || false, INSTANT_CONCURRENCY, LIST_BUDGET, query)
    }

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
                    precision: ValuePrecision::F64,
                },
            )]),
        }
    }

    /// A 1-step `[t, y, x]` grid of 2 × 2 cells, every cell `v`.
    fn grid(hour: u32, v: f64) -> QueryResult {
        step_grid(Some(hour), v, None)
    }

    /// A gridded engine's answer to an instant: 2 × 2 cells, every cell
    /// `v`, on the one-step `t` axis of the step it served (#1412), or
    /// without `t` from a collection without time; with `levels`, on a `z`
    /// axis of those levels too.
    fn step_grid(step: Option<u32>, v: f64, levels: Option<&[f64]>) -> QueryResult {
        let mut shape = vec![2, 2];
        let mut axis_names: Vec<String> = vec!["y".into(), "x".into()];
        let z = levels.map(|levels| {
            shape.insert(0, levels.len());
            axis_names.insert(0, "z".into());
            VerticalCoord {
                kind: VerticalKind::Pressure,
                values: levels.to_vec(),
            }
        });
        if step.is_some() {
            shape.insert(0, 1);
            axis_names.insert(0, "t".into());
        }
        let cells = shape.iter().product();
        QueryResult {
            domain: DomainDescription::Grid {
                x: vec![24.0, 25.0],
                y: vec![60.0, 61.0],
                t: step.map(|hour| vec![at(hour)]),
                z,
                coarsened: None,
            },
            parameters: param(),
            ranges: HashMap::from([(
                "temperature".to_string(),
                NdArray {
                    shape,
                    axis_names,
                    values: vec![Some(v); cells],
                    precision: ValuePrecision::F64,
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
                    precision: ValuePrecision::F64,
                },
            )]),
        }
    }

    /// The response is valid CoverageJSON (root CLAUDE.md Critical Rule 12).
    fn assert_valid_covjson(response: &CoverageResponse) {
        let schema: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../schemas/coveragejson.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let json = crate::response::coverage_response_to_json(response);
        let validator = jsonschema::Validator::new(&schema).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&json)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:?}\n{json}");
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

    /// A merged range keeps every step's digits (#1341): `f32` steps stay
    /// `f32`, and an `f64` step among them makes the range `f64`.
    #[test]
    fn merged_steps_keep_the_finest_precision() {
        let f32_series = |hour, v| {
            let mut q = series(24.0, &[(hour, v)]);
            q.ranges.get_mut("temperature").unwrap().precision = ValuePrecision::F32;
            CoverageResponse::Single(q)
        };
        let precision = |response| match merge(response) {
            CoverageResponse::Single(q) => q.ranges["temperature"].precision,
            CoverageResponse::Collection(_) => panic!("expected one series"),
        };
        assert_eq!(
            precision(vec![f32_series(1, 1.0), f32_series(2, 2.0)]),
            ValuePrecision::F32
        );
        assert_eq!(
            precision(vec![
                f32_series(1, 1.0),
                CoverageResponse::Single(series(24.0, &[(2, 2.0)])),
            ]),
            ValuePrecision::F64
        );
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

    /// #1436: an engine fitting its area grid to the value budget fits each
    /// instant's to the list's share of it, so every instant gets the same
    /// grid and the list fits the budget a lone instant would have filled.
    #[test]
    fn instants_share_the_area_budget() {
        let polygon = ds_core::feature::parse_area_coords("0,0,40,20").unwrap();
        let alone = polygon.fit_grid(0.01, 0.01, 1, 1).unwrap().dims();
        let fitted = Arc::new(Mutex::new(Vec::new()));
        let got = listed(&[at(1), at(2), at(3), at(4)], {
            let fitted = fitted.clone();
            move |(t, _)| {
                let axes = polygon.fit_grid(0.01, 0.01, 1, 1)?;
                fitted.lock().unwrap().push(axes.dims());
                let (nx, ny) = axes.dims();
                let mut q = step_grid(Some(t.hour()), 1.0, None);
                q.domain = DomainDescription::Grid {
                    x: axes.x,
                    y: axes.y,
                    t: Some(vec![t]),
                    z: None,
                    coarsened: axes.coarsened,
                };
                let range = q.ranges.get_mut("temperature").unwrap();
                range.shape = vec![1, ny, nx];
                range.values = vec![Some(1.0); nx * ny];
                Ok(CoverageResponse::Single(q))
            }
        });
        let CoverageResponse::Single(q) = got.unwrap() else {
            panic!("the instants' grids join along t");
        };
        let grids = fitted.lock().unwrap().clone();
        assert_eq!(grids.len(), 4);
        assert!(grids.iter().all(|&g| g == grids[0]), "{grids:?}");
        let (nx, ny) = grids[0];
        assert!(4 * nx * ny <= MAX_LIST_VALUES && nx * ny < alone.0 * alone.1);
        assert_eq!(q.ranges["temperature"].shape, vec![4, ny, nx]);
        // Outside the list a query has the whole budget again.
        let polygon = ds_core::feature::parse_area_coords("0,0,40,20").unwrap();
        assert_eq!(polygon.fit_grid(0.01, 0.01, 1, 1).unwrap().dims(), alone);
    }

    /// A gridded engine answers each instant with a one-step grid at the
    /// step it served; the list keeps every instant's step, joined along
    /// `t`, the instant without data dropping out.
    #[test]
    fn one_step_grids_join_along_t_per_instant() {
        let got = listed(&[at(1), at(2), at(3)], |(t, _): Window| {
            if t == at(2) {
                return Err(DataServerError::LocationNotFound("no step".into()));
            }
            let hour = t.hour();
            Ok(CoverageResponse::Single(step_grid(
                Some(hour),
                f64::from(hour),
                None,
            )))
        })
        .unwrap();
        let CoverageResponse::Single(q) = got else {
            panic!("expected one grid");
        };
        assert_eq!(times(&q), vec![at(1), at(3)]);
        let range = &q.ranges["temperature"];
        assert_eq!(range.shape, vec![2, 2, 2]);
        assert_eq!(range.axis_names, ["t", "y", "x"]);
        let mut expected = vec![Some(1.0); 4];
        expected.extend(vec![Some(3.0); 4]);
        assert_eq!(values(&q), expected);
    }

    #[test]
    fn one_step_grids_with_levels_join_as_t_z_y_x() {
        let levels = [850.0, 500.0];
        let got = listed(&[at(1), at(2)], move |(t, _): Window| {
            let hour = t.hour();
            Ok(CoverageResponse::Single(step_grid(
                Some(hour),
                f64::from(hour),
                Some(&levels),
            )))
        })
        .unwrap();
        assert_valid_covjson(&got);
        let CoverageResponse::Single(q) = got else {
            panic!("expected one grid");
        };
        assert_eq!(times(&q), vec![at(1), at(2)]);
        let range = &q.ranges["temperature"];
        assert_eq!(range.shape, vec![2, 2, 2, 2]);
        assert_eq!(range.axis_names, ["t", "z", "y", "x"]);
        assert_eq!(range.values[..8], [Some(1.0); 8]);
        assert_eq!(range.values[8..], [Some(2.0); 8]);
    }

    /// One answering instant still says which one it was.
    #[test]
    fn one_answering_instant_keeps_its_time() {
        let got = listed(&[at(1), at(2)], |(t, _): Window| {
            if t == at(1) {
                Err(DataServerError::LocationNotFound("no step".into()))
            } else {
                Ok(CoverageResponse::Single(step_grid(Some(2), 2.0, None)))
            }
        })
        .unwrap();
        assert_valid_covjson(&got);
        let CoverageResponse::Single(q) = got else {
            panic!("expected one grid");
        };
        assert_eq!(times(&q), vec![at(2)]);
        assert_eq!(q.ranges["temperature"].shape, vec![1, 2, 2]);
    }

    /// A collection without time answers every instant with the same grid
    /// without `t`: the list is that one grid, never a `t` axis made of the
    /// listed instants, which are no steps the collection holds.
    #[test]
    fn a_grid_without_time_stays_one_grid_without_t() {
        let got = listed(&[at(1), at(2)], |_| {
            Ok(CoverageResponse::Single(step_grid(None, 5.0, None)))
        })
        .unwrap();
        assert_valid_covjson(&got);
        let CoverageResponse::Single(q) = got else {
            panic!("expected one grid");
        };
        assert!(matches!(q.domain, DomainDescription::Grid { t: None, .. }));
        let range = &q.ranges["temperature"];
        assert_eq!(range.shape, vec![2, 2]);
        assert_eq!(range.axis_names, ["y", "x"]);
        assert_eq!(values(&q), vec![Some(5.0); 4]);
    }

    /// Grids without `t` that differ in their values are two answers, not a
    /// duplicate: the list keeps both in a collection rather than only the
    /// first instant's values.
    #[test]
    fn grids_without_time_with_different_values_stay_apart() {
        let got = listed(&[at(1), at(2)], |(t, _): Window| {
            Ok(CoverageResponse::Single(step_grid(
                None,
                f64::from(t.hour()),
                None,
            )))
        })
        .unwrap();
        assert_valid_covjson(&got);
        let CoverageResponse::Collection(v) = got else {
            panic!("two different grids are a collection");
        };
        assert_eq!(v.len(), 2);
        assert_eq!(values(&v[0]), vec![Some(1.0); 4]);
        assert_eq!(values(&v[1]), vec![Some(2.0); 4]);
        assert!(v
            .iter()
            .all(|q| matches!(q.domain, DomainDescription::Grid { t: None, .. })));
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

    /// An along-path trajectory at one step (#926): `[t, x, y]` nodes.
    fn trajectory(hour: u32) -> QueryResult {
        let t = at(hour);
        QueryResult {
            domain: DomainDescription::Trajectory {
                nodes: vec![(t, 24.0, 60.0), (t, 25.0, 61.0)],
                node_z: None,
                z: None,
            },
            parameters: param(),
            ranges: HashMap::from([(
                "temperature".to_string(),
                NdArray {
                    shape: vec![2],
                    axis_names: vec!["composite".into()],
                    values: vec![Some(270.0), Some(271.0)],
                    precision: ValuePrecision::F64,
                },
            )]),
        }
    }

    #[test]
    fn trajectories_list_per_instant_and_duplicates_drop() {
        let merged = merge(vec![
            CoverageResponse::Single(trajectory(1)),
            CoverageResponse::Single(trajectory(2)),
            CoverageResponse::Single(trajectory(2)),
        ]);
        let CoverageResponse::Collection(v) = merged else {
            panic!("trajectories at two steps are two coverages");
        };
        assert_eq!(v.len(), 2);
        assert!(v
            .iter()
            .all(|q| matches!(q.domain, DomainDescription::Trajectory { .. })));
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
        let got = listed(&[at(1), at(2), at(3)], |(start, end): Window| {
            assert_eq!(start, end, "every instant is queried as (t, t)");
            if start == at(2) {
                Err(DataServerError::LocationNotFound("no data".into()))
            } else {
                Ok(CoverageResponse::Single(series(
                    24.0,
                    &[(start.format("%H").to_string().parse().unwrap(), 1.0)],
                )))
            }
        })
        .unwrap();
        let CoverageResponse::Single(q) = got else {
            panic!("expected one series");
        };
        assert_eq!(times(&q), vec![at(1), at(3)]);
    }

    #[test]
    fn no_data_at_any_instant_is_the_first_not_found() {
        let err = listed(&[at(1), at(2)], |(t, _): Window| {
            Err(DataServerError::LocationNotFound(t.to_rfc3339()))
        })
        .unwrap_err();
        assert!(matches!(err, DataServerError::LocationNotFound(m) if m == at(1).to_rfc3339()));
    }

    #[test]
    fn other_errors_fail_the_list() {
        let err = listed(&[at(1), at(2)], |(t, _): Window| {
            if t == at(2) {
                Err(DataServerError::InvalidParameter("bad".into()))
            } else {
                Ok(CoverageResponse::Single(series(24.0, &[(1, 1.0)])))
            }
        })
        .unwrap_err();
        assert!(matches!(err, DataServerError::InvalidParameter(_)));
    }

    #[test]
    fn expired_budget_stops_before_the_next_instant() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let err = query_instants(
            &[at(1), at(2)],
            || true,
            INSTANT_CONCURRENCY,
            LIST_BUDGET,
            move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(CoverageResponse::Single(series(24.0, &[(1, 1.0)])))
            },
        )
        .unwrap_err();
        assert!(matches!(err, DataServerError::DeadlineExceeded));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn combined_values_are_bounded() {
        let big = || {
            let mut q = series(24.0, &[(1, 1.0)]);
            let range = q.ranges.get_mut("temperature").unwrap();
            range.values = vec![None; MAX_LIST_VALUES / 2 + 1];
            CoverageResponse::Single(q)
        };
        let err = listed(&[at(1), at(2)], move |_| Ok(big())).unwrap_err();
        assert!(matches!(err, DataServerError::QueryTooLarge(_)));
    }

    /// An answer of `values` values at the instant's hour, as a one-step
    /// series.
    fn sized(t: DateTime<Utc>, values: usize) -> CoverageResponse {
        let mut q = series(24.0, &[(t.hour(), f64::from(t.hour()))]);
        let range = q.ranges.get_mut("temperature").unwrap();
        range.shape = vec![values];
        range.values = vec![Some(1.0); values];
        CoverageResponse::Single(q)
    }

    /// Instants in flight now, and the most there were at once.
    #[derive(Default)]
    struct InFlight {
        now: AtomicUsize,
        peak: AtomicUsize,
        started: AtomicUsize,
    }

    impl InFlight {
        /// Run `work` as one instant in flight.
        fn during<T>(&self, work: impl FnOnce() -> T) -> T {
            self.started.fetch_add(1, Ordering::SeqCst);
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let result = work();
            self.now.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    /// The hours of the instants in the order their answers arrived.
    #[derive(Clone, Default)]
    struct Arrivals(Arc<Mutex<Vec<u32>>>);

    impl Arrivals {
        fn arrived(&self, t: DateTime<Utc>) {
            self.0.lock().unwrap().push(t.hour());
        }

        /// Whether some answer arrived before an earlier instant's.
        fn out_of_order(&self) -> bool {
            !self.0.lock().unwrap().is_sorted()
        }
    }

    /// #1522: a list of eight instants runs as many at once as it may, and
    /// never more. Every instant waits until that many run at once (or ten
    /// seconds pass), so a scheduler holding fewer fails the peak instead of
    /// passing on timing; with one at a time they run in turn.
    #[test]
    fn instants_run_side_by_side_within_the_bound() {
        use std::sync::Condvar;
        let instants: Vec<_> = (1..=8).map(at).collect();
        for bound in [INSTANT_CONCURRENCY, 2, 1] {
            let gate = Arc::new((Mutex::new((0usize, false)), Condvar::new()));
            let peak = Arc::new(AtomicUsize::new(0));
            let got = query_instants(&instants, || false, bound, LIST_BUDGET, {
                let (gate, peak) = (gate.clone(), peak.clone());
                move |(t, _): Window| {
                    let (lock, changed) = &*gate;
                    let mut state = lock.lock().unwrap();
                    state.0 += 1;
                    peak.fetch_max(state.0, Ordering::SeqCst);
                    if state.0 >= bound {
                        state.1 = true; // the bound was reached once
                        changed.notify_all();
                    }
                    let (mut state, _) = changed
                        .wait_timeout_while(state, Duration::from_secs(10), |s| !s.1)
                        .unwrap();
                    state.0 -= 1;
                    Ok(CoverageResponse::Single(series(24.0, &[(t.hour(), 1.0)])))
                }
            })
            .unwrap();
            let CoverageResponse::Single(q) = got else {
                panic!("one series");
            };
            assert_eq!(times(&q), instants);
            assert_eq!(peak.load(Ordering::SeqCst), bound, "bound {bound}");
        }
    }

    /// The answers arrive in any order, later instants first here, and the
    /// list is still the one answering the instants in turn gives: the same
    /// JSON, coverage for coverage, with an instant without data among
    /// them, for a series, a station collection and grids.
    #[test]
    fn answers_are_those_of_one_instant_after_another() {
        let instants: Vec<_> = (1..=6).map(at).collect();
        type Answer = fn(DateTime<Utc>) -> Result<CoverageResponse, DataServerError>;
        let answers: [Answer; 3] = [
            |t| Ok(CoverageResponse::Single(series(24.0, &[(t.hour(), 1.0)]))),
            |t| {
                Ok(CoverageResponse::Collection(vec![
                    series(24.0, &[(t.hour(), f64::from(t.hour()))]),
                    series(25.0, &[(t.hour(), -f64::from(t.hour()))]),
                ]))
            },
            |t| {
                Ok(CoverageResponse::Single(grid(
                    t.hour(),
                    f64::from(t.hour()),
                )))
            },
        ];
        for answer in answers {
            let json = |concurrency| {
                let arrivals = Arrivals::default();
                let got = query_instants(&instants, || false, concurrency, LIST_BUDGET, {
                    let arrivals = arrivals.clone();
                    move |(t, _): Window| {
                        // The later the instant, the sooner it answers.
                        std::thread::sleep(Duration::from_millis(10 * u64::from(8 - t.hour())));
                        arrivals.arrived(t);
                        if t == at(3) {
                            return Err(DataServerError::LocationNotFound("none".into()));
                        }
                        answer(t)
                    }
                })
                .unwrap();
                let json = crate::response::coverage_response_to_json(&got).to_string();
                (json, arrivals.out_of_order())
            };
            let (in_turn, shuffled) = json(1);
            assert!(!shuffled, "one at a time, in order");
            let (side_by_side, shuffled) = json(INSTANT_CONCURRENCY);
            assert!(shuffled, "the premise: the answers arrived out of order");
            assert_eq!(side_by_side, in_turn);
            assert!(!in_turn.contains(&at(3).to_rfc3339()), "{in_turn}");
        }
    }

    /// The error is the one the instants give in turn, whichever arrives
    /// first: the first instant's error, the first instant without data's
    /// 404, and the budget's 400 at the instant that passes it, before a
    /// later instant's error that arrived first.
    #[test]
    fn the_first_instant_in_turn_decides_the_error() {
        let instants: Vec<_> = (1..=4).map(at).collect();
        let run = |query: fn(DateTime<Utc>) -> Result<CoverageResponse, DataServerError>| {
            let arrivals = Arrivals::default();
            let err = query_instants(&instants, || false, INSTANT_CONCURRENCY, LIST_BUDGET, {
                let arrivals = arrivals.clone();
                move |(t, _): Window| {
                    // The earlier the instant, the later it answers.
                    std::thread::sleep(Duration::from_millis(20 * u64::from(5 - t.hour())));
                    arrivals.arrived(t);
                    query(t)
                }
            })
            .unwrap_err();
            assert!(
                arrivals.out_of_order(),
                "the premise: a later answer came first"
            );
            err
        };
        let err = run(|t| Err(DataServerError::InvalidParameter(t.to_rfc3339())));
        assert!(
            matches!(&err, DataServerError::InvalidParameter(m) if *m == at(1).to_rfc3339()),
            "{err:?}"
        );
        let err = run(|t| Err(DataServerError::LocationNotFound(t.to_rfc3339())));
        assert!(
            matches!(&err, DataServerError::LocationNotFound(m) if *m == at(1).to_rfc3339()),
            "{err:?}"
        );
        // In turn, the second instant passes the budget, before the fourth's
        // error, which arrives first.
        let err = run(|t| {
            if t == at(4) {
                Err(DataServerError::InvalidParameter("later".into()))
            } else {
                Ok(sized(t, MAX_LIST_VALUES / 2 + 1))
            }
        });
        assert_eq!(err.to_string(), list_too_large().to_string());
        // A query type's own budget refuses with its own 400.
        let err = query_instants(
            &instants,
            || false,
            INSTANT_CONCURRENCY,
            ValueBudget {
                max: 10,
                refuse: || DataServerError::QueryTooLarge("own".into()),
            },
            |(t, _): Window| Ok(sized(t, 6)),
        )
        .unwrap_err();
        assert!(
            matches!(&err, DataServerError::QueryTooLarge(m) if m == "own"),
            "{err:?}"
        );
    }

    /// A deciding error waits for the instants running beside it. Here,
    /// outside the executor, it stays the list's answer when one of them
    /// outlives the deadline: the first instant's `Unavailable` at once, the
    /// second taking 300 ms past a 150 ms deadline, is the 503 it is in
    /// turn, not this function's 504. Through EDR the executor answers 504
    /// at the deadline itself, before the sibling ends. With the slow
    /// instant first the list is the 504, as in turn: it ran out of time
    /// before the second instant.
    #[test]
    fn a_deciding_error_outlives_the_deadline_of_a_running_sibling() {
        let instants = [at(1), at(2)];
        let list = |slow: DateTime<Utc>| {
            let deadline = Instant::now() + Duration::from_millis(150);
            let _deadline = ds_core::deadline::enter(Some(deadline));
            let started = Instant::now();
            let got = query_instants(
                &instants,
                move || Instant::now() >= deadline,
                2,
                LIST_BUDGET,
                {
                    move |(t, _): Window| {
                        if t == slow {
                            std::thread::sleep(Duration::from_millis(300));
                            Ok(CoverageResponse::Single(series(24.0, &[(t.hour(), 1.0)])))
                        } else {
                            Err(DataServerError::Unavailable("store".into()))
                        }
                    }
                },
            );
            (got.unwrap_err(), started.elapsed())
        };
        let (err, elapsed) = list(at(2));
        assert!(matches!(err, DataServerError::Unavailable(_)), "{err:?}");
        assert!(
            elapsed >= Duration::from_millis(300),
            "the error waited for its running sibling: {elapsed:?}"
        );
        let (err, _) = list(at(1));
        assert!(matches!(err, DataServerError::DeadlineExceeded), "{err:?}");
    }

    /// Instants side by side compete for a fail-fast byte budget that one
    /// at a time never shared (#1522 review): here one that fits one
    /// instant. The instants it refused while others ran are asked again
    /// in turn after them, so the list answers as it does in turn, every
    /// instant included; a refusal in turn is the answer.
    #[test]
    fn instants_refused_beside_others_are_asked_again_in_turn() {
        let instants: Vec<_> = (1..=6).map(at).collect();
        let budget = Arc::new(AtomicUsize::new(0));
        let refused = Arc::new(AtomicUsize::new(0));
        let list = |concurrency| {
            query_instants(&instants, || false, concurrency, LIST_BUDGET, {
                let (budget, refused) = (budget.clone(), refused.clone());
                move |(t, _): Window| {
                    if budget.fetch_add(1, Ordering::SeqCst) > 0 {
                        budget.fetch_sub(1, Ordering::SeqCst);
                        refused.fetch_add(1, Ordering::SeqCst);
                        return Err(DataServerError::ResourceExhausted);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    budget.fetch_sub(1, Ordering::SeqCst);
                    Ok(CoverageResponse::Single(series(24.0, &[(t.hour(), 1.0)])))
                }
            })
            .map(|got| crate::response::coverage_response_to_json(&got).to_string())
        };
        let in_turn = list(1).unwrap();
        assert_eq!(refused.load(Ordering::SeqCst), 0);
        let side_by_side = list(INSTANT_CONCURRENCY).unwrap();
        assert!(
            refused.load(Ordering::SeqCst) > 0,
            "the premise: refusals side by side"
        );
        assert_eq!(side_by_side, in_turn);

        // A budget that refuses every instant: in turn the first refusal is
        // the answer, after the four refused side by side and that one.
        let calls = Arc::new(AtomicUsize::new(0));
        let err = query_instants(&instants, || false, INSTANT_CONCURRENCY, LIST_BUDGET, {
            let calls = calls.clone();
            move |_: Window| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(DataServerError::ResourceExhausted)
            }
        })
        .unwrap_err();
        assert!(matches!(err, DataServerError::ResourceExhausted), "{err:?}");
        assert_eq!(calls.load(Ordering::SeqCst), INSTANT_CONCURRENCY + 1);
    }

    /// `ResourceExhausted` is also a store read that failed after its
    /// retries (GeoTIFF, ODIM, NetCDF), each attempt as slow as the store:
    /// a refused instant is asked again in turn only when the time left
    /// covers the time its refusal took. Four instants whose reads fail
    /// after 200 ms, a 300 ms deadline: side by side the refusal is the
    /// 503 at about 200 ms, as in turn, with no fifth call and never a
    /// retry's 504.
    #[test]
    fn a_refusal_without_time_for_another_attempt_is_the_answer() {
        let instants: Vec<_> = (1..=4).map(at).collect();
        let delay = Duration::from_millis(200);
        for concurrency in [1, INSTANT_CONCURRENCY] {
            let deadline = Instant::now() + Duration::from_millis(300);
            let _deadline = ds_core::deadline::enter(Some(deadline));
            let calls = Arc::new(AtomicUsize::new(0));
            let started = Instant::now();
            let err = query_instants(
                &instants,
                move || Instant::now() >= deadline,
                concurrency,
                LIST_BUDGET,
                {
                    let calls = calls.clone();
                    move |_: Window| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(delay);
                        Err(DataServerError::ResourceExhausted)
                    }
                },
            )
            .unwrap_err();
            let elapsed = started.elapsed();
            assert!(
                matches!(err, DataServerError::ResourceExhausted),
                "{concurrency}: {err:?}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                concurrency,
                "no attempt after them"
            );
            assert!(elapsed < delay + delay / 2, "{concurrency}: {elapsed:?}");
        }
    }

    /// An answer deciding the list stops it: no instant starts after it,
    /// and those running finish first. The first two instants start
    /// together; the first fails at once, so the second is the last.
    #[test]
    fn a_decided_list_starts_no_more_instants() {
        let instants: Vec<_> = (1..=10).map(at).collect();
        let flight = Arc::new(InFlight::default());
        let finished = Arc::new(AtomicUsize::new(0));
        let err = query_instants(&instants, || false, 2, LIST_BUDGET, {
            let (flight, finished) = (flight.clone(), finished.clone());
            move |(t, _): Window| {
                flight.during(|| {
                    if t == at(1) {
                        return Err(DataServerError::InvalidParameter("first".into()));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    finished.fetch_add(1, Ordering::SeqCst);
                    Ok(CoverageResponse::Single(series(24.0, &[(t.hour(), 1.0)])))
                })
            }
        })
        .unwrap_err();
        assert!(
            matches!(err, DataServerError::InvalidParameter(_)),
            "{err:?}"
        );
        assert_eq!(flight.started.load(Ordering::SeqCst), 2);
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "the running one finished"
        );
    }

    /// Past its time no instant starts and the list is `DeadlineExceeded`,
    /// EDR's 504: once the list's budget has expired (the first instant
    /// expires it here, so only the two started together run), and once
    /// the request deadline passes mid-list.
    #[test]
    fn past_its_time_a_list_starts_no_more_instants() {
        let instants: Vec<_> = (1..=8).map(at).collect();
        let expired = Arc::new(AtomicBool::new(false));
        let flight = Arc::new(InFlight::default());
        let err = query_instants(
            &instants,
            {
                let expired = expired.clone();
                move || expired.load(Ordering::SeqCst)
            },
            2,
            LIST_BUDGET,
            {
                let (expired, flight) = (expired.clone(), flight.clone());
                move |(t, _): Window| {
                    flight.during(|| {
                        expired.store(true, Ordering::SeqCst);
                        Ok(CoverageResponse::Single(series(24.0, &[(t.hour(), 1.0)])))
                    })
                }
            },
        )
        .unwrap_err();
        assert!(matches!(err, DataServerError::DeadlineExceeded), "{err:?}");
        assert_eq!(flight.started.load(Ordering::SeqCst), 2);

        // 100 ms per instant, two at a time, a deadline at 150 ms: the
        // second pair starts at 100 ms and ends past it; none starts after.
        let delay = Duration::from_millis(100);
        let deadline = Instant::now() + delay * 3 / 2;
        let _deadline = ds_core::deadline::enter(Some(deadline));
        let flight = Arc::new(InFlight::default());
        let err = query_instants(
            &instants,
            move || Instant::now() >= deadline,
            2,
            LIST_BUDGET,
            {
                let flight = flight.clone();
                move |(t, _): Window| {
                    flight.during(|| {
                        std::thread::sleep(delay);
                        Ok(CoverageResponse::Single(series(24.0, &[(t.hour(), 1.0)])))
                    })
                }
            },
        )
        .unwrap_err();
        assert!(matches!(err, DataServerError::DeadlineExceeded), "{err:?}");
        assert!(flight.started.load(Ordering::SeqCst) <= 4);
    }

    /// Engines read each instant's fields as field jobs of their own
    /// (GRIB, NetCDF, GeoTIFF, ODIM): a list's jobs then run field jobs.
    /// On a runtime of two workers, eight instants four at a time, each
    /// with four field jobs that wait on the runtime's timer from inside
    /// their `block_in_place`, complete: no worker is left parked (#1200).
    #[test]
    fn instants_running_field_jobs_complete_on_a_small_runtime() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (done, finished) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let instants: Vec<_> = (1..=8).map(at).collect();
            let got = query_instants(&instants, || false, INSTANT_CONCURRENCY, LIST_BUDGET, {
                move |(t, _): Window| {
                    let mut fields = 0;
                    ds_executor::field_jobs::run_field_jobs(
                        &handle,
                        ds_executor::field_jobs::FIELD_CONCURRENCY,
                        0..4,
                        |field: usize| {
                            tokio::runtime::Handle::current()
                                .block_on(tokio::time::sleep(Duration::from_millis(5)));
                            Ok(field)
                        },
                        |_| {
                            fields += 1;
                            Ok(())
                        },
                    )?;
                    assert_eq!(fields, 4);
                    Ok(CoverageResponse::Single(series(24.0, &[(t.hour(), 1.0)])))
                }
            });
            done.send(got.map(|got| coverage_count_of(&got))).unwrap();
        });
        let got = finished
            .recv_timeout(Duration::from_secs(30))
            .expect("the list finished");
        assert_eq!(got.unwrap(), 1);
    }

    fn coverage_count_of(response: &CoverageResponse) -> usize {
        match response {
            CoverageResponse::Single(_) => 1,
            CoverageResponse::Collection(v) => v.len(),
        }
    }
}
