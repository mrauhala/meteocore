//! Bounded render admission and blocking execution shared by HTTP APIs.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub fn render_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .saturating_mul(2)
        .max(8)
}

/// Process lifetime: reload must not double the number of running renders.
pub static RENDER_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(render_concurrency())));
static QUEUE_CAPACITY: LazyLock<usize> = LazyLock::new(|| {
    env_usize(
        "MC_RENDER_QUEUE_CAPACITY",
        render_concurrency().saturating_mul(3),
    )
});
static WAITING: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(*QUEUE_CAPACITY)));
static TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
    Duration::from_millis(env_usize("MC_RENDER_TIMEOUT_MS", 3000).min(86_400_000) as u64)
});
static REJECTED: AtomicU64 = AtomicU64::new(0);
static TIMED_OUT: AtomicU64 = AtomicU64::new(0);

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v <= Semaphore::MAX_PERMITS)
        .unwrap_or(default)
}

pub struct Metrics {
    pub queued: usize,
    pub capacity: usize,
    pub rejected: u64,
    pub timed_out: u64,
}
pub fn metrics() -> Metrics {
    Metrics {
        queued: QUEUE_CAPACITY.saturating_sub(WAITING.available_permits()),
        capacity: *QUEUE_CAPACITY,
        rejected: REJECTED.load(Ordering::Relaxed),
        timed_out: TIMED_OUT.load(Ordering::Relaxed),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("Server busy, try again later")]
    Busy,
    #[error("Render deadline exceeded, try again later")]
    Timeout,
    #[error("Render task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

#[derive(Debug)]
pub struct RenderJob {
    permit: OwnedSemaphorePermit,
    deadline: Instant,
}

impl RenderJob {
    pub async fn acquire(slots: Arc<Semaphore>) -> Result<Self, ExecutionError> {
        Self::acquire_on(slots, WAITING.clone(), *TIMEOUT).await
    }

    /// 3D meshing legitimately takes longer than an interactive raster. It
    /// shares the bounded admission queue, with a separate 30-second budget.
    pub async fn acquire_volume(slots: Arc<Semaphore>) -> Result<Self, ExecutionError> {
        Self::acquire_on(slots, WAITING.clone(), Duration::from_secs(30)).await
    }

    async fn acquire_on(
        slots: Arc<Semaphore>,
        waiting: Arc<Semaphore>,
        timeout: Duration,
    ) -> Result<Self, ExecutionError> {
        let deadline = Instant::now() + timeout;
        let permit = match slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // Only actual waiters count; zero queue capacity still allows
                // immediately available CPU slots. RAII releases on disconnect.
                let _queued = waiting.try_acquire_owned().map_err(|_| {
                    REJECTED.fetch_add(1, Ordering::Relaxed);
                    ExecutionError::Busy
                })?;
                tokio::time::timeout_at(deadline.into(), slots.acquire_owned())
                    .await
                    .map_err(|_| timeout_error())?
                    .map_err(|_| ExecutionError::Busy)?
            }
        };
        Ok(Self { permit, deadline })
    }

    pub async fn run<T: Send + 'static>(
        self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, ExecutionError> {
        let deadline = self.deadline;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = self.permit;
            let _scope = ds_core::deadline::enter(Some(deadline));
            if Instant::now() >= deadline {
                return Err(ExecutionError::Timeout);
            }
            Ok(work())
        });
        // abort only cancels tasks that have not started. Running work owns its
        // permit until completion, even after HTTP timeout or disconnection.
        let _abort = AbortOnDrop(task.abort_handle());
        let result = tokio::time::timeout_at(deadline.into(), task)
            .await
            .map_err(|_| timeout_error())??;
        if Instant::now() >= deadline {
            return Err(timeout_error());
        }
        result
    }
}

fn timeout_error() -> ExecutionError {
    TIMED_OUT.fetch_add(1, Ordering::Relaxed);
    ExecutionError::Timeout
}
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queue_overflow_sheds_immediately_and_disconnect_releases_waiter() {
        let slots = Arc::new(Semaphore::new(1));
        let held = slots.clone().acquire_owned().await.unwrap();
        let waiting = Arc::new(Semaphore::new(1));
        let task = tokio::spawn(RenderJob::acquire_on(
            slots.clone(),
            waiting.clone(),
            Duration::from_secs(5),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while waiting.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let result =
            RenderJob::acquire_on(slots.clone(), waiting.clone(), Duration::from_secs(5)).await;
        assert!(matches!(result, Err(ExecutionError::Busy)));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(waiting.available_permits(), 1);
        drop(held);
        let job = RenderJob::acquire_on(
            slots.clone(),
            Arc::new(Semaphore::new(0)),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(job.run(|| 42).await.unwrap(), 42);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn timeout_keeps_running_worker_permit_until_completion() {
        let slots = Arc::new(Semaphore::new(1));
        let job = RenderJob::acquire_on(
            slots.clone(),
            Arc::new(Semaphore::new(0)),
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(job.run(move || {
            started.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(ds_core::deadline::check().is_err());
        }));
        started_rx.await.unwrap();
        assert!(matches!(task.await.unwrap(), Err(ExecutionError::Timeout)));
        assert_eq!(slots.available_permits(), 0);
        release.send(()).unwrap();
        let _released = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn queue_time_consumes_the_same_deadline_and_never_dispatches_expired_work() {
        let slots = Arc::new(Semaphore::new(0));
        let waiting = Arc::new(Semaphore::new(1));
        assert!(matches!(
            RenderJob::acquire_on(slots, waiting.clone(), Duration::from_millis(10)).await,
            Err(ExecutionError::Timeout)
        ));
        assert_eq!(waiting.available_permits(), 1);
        let job = RenderJob::acquire_on(Arc::new(Semaphore::new(1)), waiting, Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(
            job.run(|| panic!("expired work must not start")).await,
            Err(ExecutionError::Timeout)
        ));
    }
}
