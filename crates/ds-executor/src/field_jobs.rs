//! Bounded field jobs inside one admitted engine query (#1067).
//!
//! A gridded engine answers an EDR time series or area by reading one field
//! per (parameter, step, level). [`run_field_jobs`] runs those reads a few
//! at a time, as many as its caller allows ([`FIELD_CONCURRENCY`] unless it
//! has its own memory bound, #1297), as tasks on the runtime the query runs
//! on (the shared query runtime, [`crate::query_runtime`]), each inside
//! `block_in_place` and under the query's `ds_core::deadline`, and hands
//! each job's small result (samples, a subset) to the caller in completion
//! order. Taken from engine-grib, which keeps its own fallback runtime;
//! engine-netcdf shares it.

use std::future::Future;
use std::sync::Arc;

use ds_core::error::DataServerError;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::task::JoinSet;

/// The default number of field jobs in flight per admitted query, which
/// bounds its field I/O, decoding and live decoded fields: engine-netcdf's.
/// An engine that bounds a job's memory itself passes its own number to
/// [`run_field_jobs`] (engine-grib, #1297).
pub const FIELD_CONCURRENCY: usize = 4;

/// Run `future` to completion from synchronous engine code: on the calling
/// thread's runtime inside `block_in_place` (a multi-thread worker, such as
/// the query runtime's), else on `fallback`, the engine's own multi-thread
/// runtime: directly for a caller outside any runtime, and on a scoped
/// thread carrying the caller's `ds_core::deadline` for a caller on a
/// current-thread runtime (a default `#[tokio::test]`), which can neither
/// `block_in_place` nor block on another runtime.
pub fn block_on<F>(fallback: &Handle, future: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        Ok(_) => {
            let deadline = ds_core::deadline::current();
            std::thread::scope(|scope| {
                scope
                    .spawn(move || {
                        let _deadline = ds_core::deadline::enter(deadline);
                        fallback.block_on(future)
                    })
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
        }
        Err(_) => fallback.block_on(future),
    }
}

/// Run `work` over `fields`, at most `concurrency` at once (at least one;
/// [`FIELD_CONCURRENCY`] unless the engine bounds a job's memory itself), on
/// the runtime [`block_on`] picks, and pass each result to `consume` in
/// completion order. Workers should return small samples or subsets rather
/// than keep decoded fields alive.
///
/// Each job re-enters the caller's `ds_core::deadline` (a task starts
/// without it) and checks it before and after its work. On the first error,
/// from a job or from `consume`, or once the deadline passes, no further job
/// starts; the jobs already running are drained before this returns, so
/// nothing of the query outlives it here.
pub fn run_field_jobs<J, T>(
    fallback: &Handle,
    concurrency: usize,
    fields: impl Iterator<Item = J> + Send,
    work: impl Fn(J) -> Result<T, DataServerError> + Send + Sync + 'static,
    mut consume: impl FnMut(T) -> Result<(), DataServerError> + Send,
) -> Result<(), DataServerError>
where
    J: Send + 'static,
    T: Send + 'static,
{
    let concurrency = concurrency.max(1);
    let mut fields = fields.fuse();
    let work = Arc::new(work);
    let deadline = ds_core::deadline::current();
    block_on(fallback, async {
        let mut jobs = JoinSet::new();
        let mut fatal = None;
        loop {
            if let Err(error) = ds_core::deadline::check() {
                fatal = Some(error);
            }
            while jobs.len() < concurrency && fatal.is_none() {
                let Some(field) = fields.next() else { break };
                let work = work.clone();
                jobs.spawn(async move {
                    // Single-flight cache waits are synchronous as well as
                    // storage reads; free the runtime worker for the whole job.
                    tokio::task::block_in_place(|| {
                        let _deadline = ds_core::deadline::enter(deadline);
                        ds_core::deadline::check()?;
                        let result = work(field);
                        ds_core::deadline::check()?;
                        result
                    })
                });
            }
            let Some(result) = jobs.join_next().await else {
                break;
            };
            let result = result.unwrap_or_else(|e| {
                Err(DataServerError::Engine(format!("field worker failed: {e}")))
            });
            if fatal.is_none() {
                if let Err(error) = result.and_then(&mut consume) {
                    fatal = Some(error);
                }
            }
        }
        match fatal {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    fn fallback() -> &'static Handle {
        crate::query_runtime()
    }

    /// Every field once, at most four at a time, each job under the
    /// caller's deadline, from a thread outside any runtime.
    #[test]
    fn jobs_run_bounded_under_the_callers_deadline() {
        let end = Instant::now() + Duration::from_secs(60);
        let _deadline = ds_core::deadline::enter(Some(end));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (r, p) = (running.clone(), peak.clone());
        let mut seen = Vec::new();
        run_field_jobs(
            fallback(),
            FIELD_CONCURRENCY,
            0..12,
            move |field: usize| {
                let now = r.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                r.fetch_sub(1, Ordering::SeqCst);
                assert_eq!(ds_core::deadline::current(), Some(end));
                Ok(field)
            },
            |field| {
                seen.push(field);
                Ok(())
            },
        )
        .unwrap();
        seen.sort_unstable();
        assert_eq!(seen, (0..12).collect::<Vec<_>>());
        assert!((2..=FIELD_CONCURRENCY).contains(&peak.load(Ordering::SeqCst)));
    }

    /// The caller's bound, the default or an engine's own (#1297), is
    /// reached and never exceeded: every job waits until that many run at
    /// once (or ten seconds pass), so a scheduler holding fewer fails the
    /// peak check instead of passing on timing.
    #[test]
    fn the_callers_bound_is_reached_and_never_exceeded() {
        use std::sync::{Condvar, Mutex};
        for bound in [1, FIELD_CONCURRENCY, 16] {
            let gate = Arc::new((Mutex::new((0usize, false)), Condvar::new()));
            let peak = Arc::new(AtomicUsize::new(0));
            let (g, p) = (gate.clone(), peak.clone());
            let mut done = 0;
            run_field_jobs(
                fallback(),
                bound,
                0..3 * bound,
                move |_: usize| {
                    let (lock, changed) = &*g;
                    let mut state = lock.lock().unwrap();
                    state.0 += 1;
                    p.fetch_max(state.0, Ordering::SeqCst);
                    if state.0 >= bound {
                        state.1 = true; // the bound was reached once
                        changed.notify_all();
                    }
                    let (mut state, _) = changed
                        .wait_timeout_while(state, Duration::from_secs(10), |s| !s.1)
                        .unwrap();
                    state.0 -= 1;
                    Ok(())
                },
                |()| {
                    done += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(done, 3 * bound);
            assert_eq!(peak.load(Ordering::SeqCst), bound, "bound {bound}");
        }
    }

    /// The first error stops dispatching: the remaining fields never start,
    /// and the running ones finish before the call returns.
    #[test]
    fn an_error_stops_dispatch_and_drains_the_running_jobs() {
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let (s, f) = (started.clone(), finished.clone());
        let result = run_field_jobs(
            fallback(),
            FIELD_CONCURRENCY,
            0..100,
            move |field: usize| {
                s.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
                f.fetch_add(1, Ordering::SeqCst);
                if field == 0 {
                    Err(DataServerError::QueryTooLarge("stop".into()))
                } else {
                    Ok(())
                }
            },
            |()| Ok(()),
        );
        assert!(matches!(result, Err(DataServerError::QueryTooLarge(_))));
        let started = started.load(Ordering::SeqCst);
        assert!(started < 100, "{started} jobs started");
        assert_eq!(finished.load(Ordering::SeqCst), started);
    }

    /// A caller on a current-thread runtime, a default `#[tokio::test]`,
    /// which cannot `block_in_place`: the jobs run on the fallback, under
    /// the caller's deadline.
    #[test]
    fn a_current_thread_caller_runs_its_jobs_on_the_fallback() {
        let end = Instant::now() + Duration::from_secs(60);
        let current = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let seen = current.block_on(async move {
            let _deadline = ds_core::deadline::enter(Some(end));
            let mut seen = Vec::new();
            run_field_jobs(
                fallback(),
                FIELD_CONCURRENCY,
                0..6,
                move |field: usize| {
                    assert_eq!(ds_core::deadline::current(), Some(end));
                    Ok(field)
                },
                |field| {
                    seen.push(field);
                    Ok(())
                },
            )
            .map(|()| seen)
        });
        let mut seen = seen.unwrap();
        seen.sort_unstable();
        assert_eq!(seen, (0..6).collect::<Vec<_>>());
        let _deadline = ds_core::deadline::enter(Some(end));
        let value = current
            .block_on(async { block_on(fallback(), async { ds_core::deadline::current() }) });
        assert_eq!(value, Some(end));
    }

    /// A passed deadline starts nothing.
    #[test]
    fn a_passed_deadline_starts_no_job() {
        let _deadline = ds_core::deadline::enter(Some(Instant::now()));
        let result = run_field_jobs(
            fallback(),
            FIELD_CONCURRENCY,
            0..4,
            |_: usize| -> Result<(), DataServerError> { panic!("a job started") },
            |()| Ok(()),
        );
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
    }
}
