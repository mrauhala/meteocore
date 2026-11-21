//! Bounded execution of synchronous EDR engines away from HTTP workers.
//!
//! Queries run on ds-executor's shared query runtime
//! (`ds_executor::query_runtime`), the one WMS GetFeatureInfo also runs on
//! (#1103), under this module's own admission and 30-second deadline: storage
//! engines may use `block_in_place` there. Cross-section trajectory engines
//! (`TrajectoryShape::CrossSection`) explicitly expect a blocking thread, so
//! that path uses the same runtime's blocking pool.
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

static EXECUTOR: LazyLock<Executor> = LazyLock::new(Executor::new);

/// EDR's admission, apart from GetFeatureInfo's render slots: one running
/// query per query-runtime worker, 32 more waiting.
struct Executor {
    slots: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
}

impl Executor {
    fn new() -> Self {
        let concurrency = ds_executor::query_workers();
        Self {
            slots: Arc::new(Semaphore::new(concurrency)),
            admitted: Arc::new(Semaphore::new(concurrency + 32)),
        }
    }
}

#[derive(Debug)]
pub enum ExecutionError {
    Busy,
    Timeout,
    Task(tokio::task::JoinError),
}

/// Check between engine calls so a timed-out/disconnected MULTIPOINT stops
/// before launching another query. A running synchronous call cannot be killed.
pub struct QueryBudget {
    expiry: Expiry,
    /// EDR's running-query slots, of which `_permit` is this query's.
    slots: Arc<Semaphore>,
    _permit: OwnedSemaphorePermit,
    _admission: OwnedSemaphorePermit,
}

impl QueryBudget {
    pub fn expired(&self) -> bool {
        self.expiry.expired()
    }

    /// The query's [`Expiry`], for work it hands to other threads (a
    /// `datetime` list's instants, #1522).
    pub fn expiry(&self) -> Expiry {
        self.expiry.clone()
    }

    /// Up to `wanted` more of EDR's running-query slots, for a query that
    /// answers parts of itself side by side (a `datetime` list's instants,
    /// #1522): only those free now, never waiting, and never the last free
    /// one, which stays for the next query to arrive. A slot is free only
    /// when no query waits for one (a released slot goes to a waiter
    /// first), so this never overtakes a queued query, and under load the
    /// query keeps to the one slot it holds; so does every query when EDR
    /// admits two (`ds_executor::query_workers()` on two CPUs). Each part
    /// then runs in a slot of its own, so the engine queries running at
    /// once stay within EDR's admission, as separate requests would.
    pub fn spare_slots(&self, wanted: usize) -> SpareSlots {
        let mut taken = Vec::with_capacity(wanted);
        while taken.len() < wanted {
            // Two at once, one of them given back: a slot is taken only
            // while another is free, however many queries take at once.
            let Ok(mut pair) = self.slots.clone().try_acquire_many_owned(2) else {
                break;
            };
            if let Some(slot) = pair.split(1) {
                taken.push(slot);
            }
        }
        SpareSlots(taken)
    }
}

/// Whether a query is past its deadline or its client has gone: cheap to
/// clone into the work the query fans out.
#[derive(Clone)]
pub struct Expiry {
    deadline: Instant,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl Expiry {
    pub fn expired(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed) || Instant::now() >= self.deadline
    }
}

/// The slots [`QueryBudget::spare_slots`] took, given back on drop.
pub struct SpareSlots(Vec<OwnedSemaphorePermit>);

impl SpareSlots {
    /// How many slots were free.
    pub fn count(&self) -> usize {
        self.0.len()
    }
}

struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

pub async fn run<T, F>(blocking: bool, work: F) -> Result<T, ExecutionError>
where
    T: Send + 'static,
    F: FnOnce(QueryBudget) -> T + Send + 'static,
{
    run_on(&EXECUTOR, blocking, QUERY_TIMEOUT, work).await
}

async fn run_on<T, F>(
    executor: &Executor,
    blocking: bool,
    timeout: Duration,
    work: F,
) -> Result<T, ExecutionError>
where
    T: Send + 'static,
    F: FnOnce(QueryBudget) -> T + Send + 'static,
{
    let deadline = Instant::now() + timeout;
    let admission = executor
        .admitted
        .clone()
        .try_acquire_owned()
        .map_err(|_| ExecutionError::Busy)?;
    let permit = tokio::time::timeout_at(deadline.into(), executor.slots.clone().acquire_owned())
        .await
        .map_err(|_| ExecutionError::Timeout)?
        .map_err(|_| ExecutionError::Busy)?;
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _cancel = CancelOnDrop(cancelled.clone());
    let budget = QueryBudget {
        expiry: Expiry {
            deadline,
            cancelled,
        },
        slots: executor.slots.clone(),
        _permit: permit,
        _admission: admission,
    };
    let work = move || {
        let _deadline = ds_core::deadline::enter(Some(deadline));
        work(budget)
    };
    let runtime = ds_executor::query_runtime();
    let task = if blocking {
        runtime.spawn_blocking(work)
    } else {
        runtime.spawn(async move { work() })
    };
    // Keep the permit in the task even after timeout/disconnect: synchronous
    // work may still be running. Never turn a timeout into extra concurrency.
    tokio::time::timeout_at(deadline.into(), task)
        .await
        .map_err(|_| ExecutionError::Timeout)?
        .map_err(ExecutionError::Task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn engine_work_receives_the_same_absolute_deadline() {
        let executor = Executor::new();
        for blocking in [false, true] {
            run_on(&executor, blocking, Duration::from_secs(2), |budget| {
                assert_eq!(ds_core::deadline::current(), Some(budget.expiry.deadline));
                assert!(ds_core::deadline::check().is_ok());
            })
            .await
            .unwrap();
        }
    }

    /// A query takes only the slots free now beside its own (#1522), never
    /// waiting for one and never the last free one, and gives them back
    /// when it is done with them.
    #[tokio::test]
    async fn spare_slots_are_the_free_ones_but_the_last_and_given_back() {
        let spare = |slots: usize, wanted: usize| {
            let executor = Executor {
                slots: Arc::new(Semaphore::new(slots)),
                admitted: Arc::new(Semaphore::new(8)),
            };
            async move {
                let taken = run_on(&executor, false, QUERY_TIMEOUT, move |budget| {
                    let spare = budget.spare_slots(wanted);
                    (spare.count(), budget.spare_slots(wanted).count())
                })
                .await
                .unwrap();
                assert_eq!(executor.slots.available_permits(), slots, "all given back");
                taken
            }
        };
        // Two slots, the fewest EDR admits: the query keeps to its own.
        assert_eq!(spare(2, 3).await, (0, 0));
        assert_eq!(spare(3, 3).await, (1, 0), "one free beside the last");
        assert_eq!(spare(4, 3).await, (2, 0));
        assert_eq!(spare(5, 3).await, (3, 0), "as many as wanted");
        assert_eq!(
            spare(6, 3).await,
            (3, 1),
            "a second taker leaves the last too"
        );

        let executor = Executor {
            slots: Arc::new(Semaphore::new(3)),
            admitted: Arc::new(Semaphore::new(8)),
        };
        let _busy = executor.slots.clone().acquire_many_owned(2).await.unwrap();
        let busy = run_on(&executor, false, QUERY_TIMEOUT, |budget| {
            budget.spare_slots(5).count()
        })
        .await
        .unwrap();
        assert_eq!(
            busy, 0,
            "every other slot taken: the query keeps to its own"
        );
        assert_eq!(executor.slots.available_permits(), 1);
    }

    /// A query waiting for a slot is never overtaken (#1522): with one
    /// queued, a running query takes no spare slot, and slots given back go
    /// to the waiting query first. Two of three held slots given back leave
    /// one free, the last, once the waiting query has its own, so the
    /// running query still takes none; had they gone to it, it would take
    /// one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spare_slots_never_overtake_a_waiting_query() {
        let executor = Arc::new(Executor {
            slots: Arc::new(Semaphore::new(4)),
            admitted: Arc::new(Semaphore::new(8)),
        });
        let held: Vec<_> = (0..3)
            .map(|_| executor.slots.clone().try_acquire_owned().unwrap())
            .collect();
        let (give, given) = std::sync::mpsc::channel::<Vec<OwnedSemaphorePermit>>();
        let running = tokio::spawn({
            let executor = executor.clone();
            async move {
                run_on(&executor, false, QUERY_TIMEOUT, move |budget| {
                    let mut held = given.recv_timeout(Duration::from_secs(10)).unwrap();
                    let queued = budget.spare_slots(3).count();
                    held.truncate(1);
                    let after = budget.spare_slots(3).count();
                    (queued, after)
                })
                .await
                .unwrap()
            }
        });
        while executor.slots.available_permits() > 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let waiting = tokio::spawn({
            let executor = executor.clone();
            async move { run_on(&executor, false, QUERY_TIMEOUT, |_| ()).await }
        });
        // Admitted, the waiting query is queued for a slot in the same poll.
        while executor.admitted.available_permits() > 6 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        give.send(held).unwrap();
        let (queued, after) = running.await.unwrap();
        assert_eq!(queued, 0, "a query waits: no slot is free");
        assert_eq!(
            after, 0,
            "the given-back slots went to the waiting query first"
        );
        waiting.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn timed_out_work_keeps_its_slot_and_stops_fanout() {
        let executor = Executor {
            slots: Arc::new(Semaphore::new(1)),
            admitted: Arc::new(Semaphore::new(1)),
        };
        let (release, wait) = std::sync::mpsc::channel();
        let (stopped, finished) = tokio::sync::oneshot::channel();
        let result = run_on(&executor, false, Duration::from_millis(20), move |budget| {
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            stopped.send(budget.expired()).unwrap();
        })
        .await;
        assert!(matches!(result, Err(ExecutionError::Timeout)));
        assert!(matches!(
            run_on(&executor, false, QUERY_TIMEOUT, |_| ()).await,
            Err(ExecutionError::Busy)
        ));
        release.send(()).unwrap();
        assert!(finished.await.unwrap());
    }

    #[tokio::test]
    async fn queue_deadline_never_dispatches_work() {
        let executor = Executor {
            slots: Arc::new(Semaphore::new(0)),
            ..Executor::new()
        };
        let result = run_on(&executor, false, Duration::from_millis(1), |_| {
            panic!("expired queued query must not call the engine")
        })
        .await;
        assert!(matches!(result, Err(ExecutionError::Timeout)));
    }

    /// Every EDR query, a cross-section's blocking one too, runs on the
    /// query runtime WMS GetFeatureInfo shares (#1103), never on the caller's,
    /// under EDR's own 30-second deadline, not GetFeatureInfo's or the render
    /// deadline.
    #[tokio::test(flavor = "current_thread")]
    async fn queries_run_on_the_shared_query_runtime_under_the_edr_deadline() {
        let caller = tokio::runtime::Handle::current().id();
        for blocking in [false, true] {
            let start = Instant::now();
            let (runtime, thread, deadline) = run(blocking, |_| {
                (
                    tokio::runtime::Handle::current().id(),
                    std::thread::current().name().map(str::to_owned),
                    ds_core::deadline::current().unwrap(),
                )
            })
            .await
            .unwrap();
            assert_eq!(runtime, ds_executor::query_runtime().id(), "{blocking}");
            assert_ne!(runtime, caller, "{blocking}");
            assert_eq!(thread.as_deref(), Some(ds_executor::QUERY_THREAD_NAME));
            let budget = deadline - start;
            assert_eq!(QUERY_TIMEOUT, Duration::from_secs(30));
            assert!(
                QUERY_TIMEOUT <= budget && budget < QUERY_TIMEOUT + Duration::from_secs(1),
                "{budget:?}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn synchronous_work_does_not_block_http_runtime_and_supports_storage_bridge() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let query = tokio::spawn(run(false, move |_budget| {
            started_tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    42
                })
            })
        }));
        started_rx.await.unwrap();
        // This task can progress while the synchronous engine is blocked.
        finish_tx.send(()).unwrap();
        assert_eq!(query.await.unwrap().unwrap(), 42);
    }
}
