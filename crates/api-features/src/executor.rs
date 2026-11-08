//! Engine reads off the request workers (#503).
//!
//! An engine whose `get_features`/`get_feature` answer from memory is
//! called in place, as before. One that reads a database per call
//! (`FeatureEngine::reads_on_request`: engine-postgis `events`) runs on
//! ds-executor's shared query runtime (`ds_executor::query_runtime`), where
//! `block_in_place` is valid, under this module's own admission and
//! deadline: one running read per query-runtime worker, 32 more waiting,
//! 30 seconds. It is the twin of api-edr's `executor`, which bounds EDR's
//! queries on the same runtime; the two admissions are separate, as root
//! CLAUDE.md asks of each API.
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use ds_core::error::DataServerError;
use ds_core::feature_engine::FeatureEngine;
use tokio::sync::Semaphore;

/// How long one read may take, its wait for a slot included.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

static ADMISSION: LazyLock<Admission> = LazyLock::new(Admission::new);

struct Admission {
    slots: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
}

impl Admission {
    fn new() -> Self {
        let concurrency = ds_executor::query_workers();
        Self {
            slots: Arc::new(Semaphore::new(concurrency)),
            admitted: Arc::new(Semaphore::new(concurrency + 32)),
        }
    }
}

/// Run `read` against `engine`: in place when the engine answers from
/// memory, else on the query runtime. A full queue is
/// `ResourceExhausted`, a read past [`READ_TIMEOUT`] `DeadlineExceeded`
/// (both a 503 with `Retry-After`), a read that panicked an opaque engine
/// error.
pub(crate) async fn read<T, F>(
    engine: &Arc<dyn FeatureEngine>,
    read: F,
) -> Result<T, DataServerError>
where
    T: Send + 'static,
    F: FnOnce(&dyn FeatureEngine) -> Result<T, DataServerError> + Send + 'static,
{
    if !engine.reads_on_request() {
        return read(engine.as_ref());
    }
    let engine = Arc::clone(engine);
    run_on(&ADMISSION, READ_TIMEOUT, move || read(engine.as_ref())).await
}

async fn run_on<T, F>(
    admission: &Admission,
    timeout: Duration,
    work: F,
) -> Result<T, DataServerError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, DataServerError> + Send + 'static,
{
    let deadline = Instant::now() + timeout;
    let admitted = admission
        .admitted
        .clone()
        .try_acquire_owned()
        .map_err(|_| DataServerError::ResourceExhausted)?;
    let slot = tokio::time::timeout_at(deadline.into(), admission.slots.clone().acquire_owned())
        .await
        .map_err(|_| DataServerError::DeadlineExceeded)?
        .map_err(|_| DataServerError::ResourceExhausted)?;
    // The permits move into the task: a read still running after the
    // deadline or a client disconnect keeps its slot until it returns, so a
    // timeout never turns into extra concurrency.
    let task = ds_executor::query_runtime().spawn(async move {
        let _permits = (admitted, slot);
        if Instant::now() >= deadline {
            return Err(DataServerError::DeadlineExceeded);
        }
        let _deadline = ds_core::deadline::enter(Some(deadline));
        work()
    });
    match tokio::time::timeout_at(deadline.into(), task).await {
        Err(_) => Err(DataServerError::DeadlineExceeded),
        Ok(Err(e)) => {
            tracing::error!("Features read task failed: {e}");
            Err(DataServerError::Engine("features read task failed".into()))
        }
        Ok(Ok(result)) => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission(running: usize, waiting: usize) -> Admission {
        Admission {
            slots: Arc::new(Semaphore::new(running)),
            admitted: Arc::new(Semaphore::new(running + waiting)),
        }
    }

    #[tokio::test]
    async fn a_read_runs_on_the_query_runtime_under_its_deadline() {
        let (thread, deadline) = run_on(&admission(1, 0), READ_TIMEOUT, || {
            Ok((
                std::thread::current().name().map(str::to_owned),
                ds_core::deadline::current(),
            ))
        })
        .await
        .unwrap();
        assert!(
            thread
                .as_deref()
                .is_some_and(|name| name.starts_with(ds_executor::QUERY_THREAD_NAME)),
            "{thread:?}"
        );
        assert!(deadline.is_some());
    }

    #[tokio::test]
    async fn a_full_queue_is_busy_and_a_slow_read_keeps_its_slot() {
        let admission = admission(1, 0);
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let slow = run_on(&admission, Duration::from_millis(20), move || {
            wait.recv_timeout(Duration::from_secs(5)).ok();
            Ok(())
        })
        .await;
        assert!(matches!(slow, Err(DataServerError::DeadlineExceeded)));
        // The timed-out read still runs and holds the only admission.
        let busy = run_on(&admission, READ_TIMEOUT, || Ok(())).await;
        assert!(matches!(busy, Err(DataServerError::ResourceExhausted)));
        release.send(()).unwrap();
    }
}
