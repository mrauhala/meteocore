//! Bounded execution of synchronous EDR engines away from HTTP workers.
//!
//! Normal queries run on a dedicated multi-thread runtime: storage engines
//! may use `block_in_place` there. Trajectory engines explicitly expect a
//! blocking thread, so that path uses the same runtime's blocking pool.
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

static EXECUTOR: LazyLock<Executor> = LazyLock::new(Executor::new);

struct Executor {
    runtime: Runtime,
    slots: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
}

impl Executor {
    fn new() -> Self {
        let concurrency = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .clamp(2, 8);
        Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(concurrency)
                .thread_name("edr-query")
                .enable_all()
                .build()
                .expect("EDR query runtime"),
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
    deadline: Instant,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    _permit: OwnedSemaphorePermit,
    _admission: OwnedSemaphorePermit,
}

impl QueryBudget {
    pub fn expired(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed) || Instant::now() >= self.deadline
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
        deadline,
        cancelled,
        _permit: permit,
        _admission: admission,
    };
    let work = move || {
        let _deadline = ds_core::deadline::enter(Some(deadline));
        work(budget)
    };
    let task = if blocking {
        executor.runtime.spawn_blocking(work)
    } else {
        executor.runtime.spawn(async move { work() })
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
                assert_eq!(ds_core::deadline::current(), Some(budget.deadline));
                assert!(ds_core::deadline::check().is_ok());
            })
            .await
            .unwrap();
        }
        executor.runtime.shutdown_background();
    }

    #[tokio::test]
    async fn timed_out_work_keeps_its_slot_and_stops_fanout() {
        let executor = Executor {
            slots: Arc::new(Semaphore::new(1)),
            admitted: Arc::new(Semaphore::new(1)),
            ..Executor::new()
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
        executor.runtime.shutdown_background();
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
        executor.runtime.shutdown_background();
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
