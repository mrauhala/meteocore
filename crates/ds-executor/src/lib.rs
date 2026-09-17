//! Bounded render admission and blocking execution shared by HTTP APIs.
pub mod budget;
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
    /// Reserve output memory and CPU together. Short bursts wait in the same
    /// bounded queue as CPU contention, without occupying a CPU slot while
    /// waiting for memory. Both waits consume the original render deadline.
    pub async fn acquire_raster(
        slots: Arc<Semaphore>,
        width: u32,
        height: u32,
    ) -> Result<(Self, Arc<budget::RenderPermit>), ExecutionError> {
        Self::acquire_raster_on(
            slots,
            WAITING.clone(),
            *TIMEOUT,
            budget::RENDER_MEMORY.clone(),
            width,
            height,
        )
        .await
    }

    async fn acquire_raster_on(
        slots: Arc<Semaphore>,
        waiting: Arc<Semaphore>,
        timeout: Duration,
        memory: Arc<budget::RenderBudget>,
        width: u32,
        height: u32,
    ) -> Result<(Self, Arc<budget::RenderPermit>), ExecutionError> {
        let deadline = Instant::now() + timeout;
        // A request that can never fit must not occupy the waiting queue.
        if !memory.fits(width, height) {
            memory.reject();
            return Err(ExecutionError::Busy);
        }
        if let Some(reservation) = memory.try_reserve(width, height) {
            if let Ok(permit) = slots.clone().try_acquire_owned() {
                return Ok((Self { permit, deadline }, Arc::new(reservation)));
            }
        }
        let _queued = waiting.try_acquire_owned().map_err(|_| {
            REJECTED.fetch_add(1, Ordering::Relaxed);
            ExecutionError::Busy
        })?;
        let reservation = tokio::time::timeout_at(deadline.into(), memory.reserve(width, height))
            .await
            .map_err(|_| {
                memory.reject();
                timeout_error()
            })?;
        let permit = tokio::time::timeout_at(deadline.into(), slots.acquire_owned())
            .await
            .map_err(|_| timeout_error())?
            .map_err(|_| ExecutionError::Busy)?;
        Ok((Self { permit, deadline }, Arc::new(reservation)))
    }

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
        // Count at the request boundary, exactly once. Counting inside the
        // worker would race the outer timeout and could count one expiry twice.
        if matches!(&result, Err(ExecutionError::Timeout)) || Instant::now() >= deadline {
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

    async fn wait_until_queued(waiting: &Semaphore, available: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while waiting.available_permits() != available {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn raster_memory_wait_is_bounded_cancellable_and_does_not_hold_cpu() {
        let memory = Arc::new(budget::RenderBudget::new(budget::BYTES_PER_PIXEL));
        let held = memory.try_reserve(1, 1).unwrap();
        let slots = Arc::new(Semaphore::new(24));
        let waiting = Arc::new(Semaphore::new(1));
        let task = tokio::spawn(RenderJob::acquire_raster_on(
            slots.clone(),
            waiting.clone(),
            Duration::from_secs(2),
            memory.clone(),
            1,
            1,
        ));
        wait_until_queued(&waiting, 0).await;
        assert_eq!(slots.available_permits(), 24);
        assert!(matches!(
            RenderJob::acquire_raster_on(
                slots.clone(),
                waiting.clone(),
                Duration::from_secs(2),
                memory.clone(),
                1,
                1,
            )
            .await,
            Err(ExecutionError::Busy)
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(waiting.available_permits(), 1);
        assert_eq!(memory.available(), 0);
        drop(held);
        assert_eq!(memory.available(), memory.capacity());
    }

    #[tokio::test]
    async fn timeline_burst_waits_for_memory_and_all_frames_complete() {
        // Production regression: 24 CPU slots but only six full-size frames
        // fit in output memory. Previously the other seven failed immediately.
        let memory = Arc::new(budget::RenderBudget::new(6 * budget::BYTES_PER_PIXEL));
        let slots = Arc::new(Semaphore::new(24));
        let waiting = Arc::new(Semaphore::new(72));
        let mut initial = Vec::new();
        for _ in 0..6 {
            initial.push(
                RenderJob::acquire_raster_on(
                    slots.clone(),
                    waiting.clone(),
                    Duration::from_secs(2),
                    memory.clone(),
                    1,
                    1,
                )
                .await
                .unwrap(),
            );
        }
        let mut tasks = Vec::new();
        for _ in 0..7 {
            let (slots, waiting, memory) = (slots.clone(), waiting.clone(), memory.clone());
            tasks.push(tokio::spawn(async move {
                let (job, reservation) = RenderJob::acquire_raster_on(
                    slots,
                    waiting,
                    Duration::from_secs(2),
                    memory,
                    1,
                    1,
                )
                .await
                .unwrap();
                job.run(move || {
                    let _reservation = reservation;
                    1
                })
                .await
                .unwrap()
            }));
        }
        wait_until_queued(&waiting, 65).await;
        for (job, reservation) in initial {
            assert_eq!(
                job.run(move || {
                    let _reservation = reservation;
                    1
                })
                .await
                .unwrap(),
                1
            );
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), 1);
        }
        assert_eq!(memory.available(), memory.capacity());
        assert_eq!(memory.rejected(), 0);
        assert_eq!(slots.available_permits(), 24);
        assert_eq!(waiting.available_permits(), 72);
    }

    #[tokio::test]
    async fn impossible_requests_and_memory_deadlines_release_admission() {
        let memory = Arc::new(budget::RenderBudget::new(budget::BYTES_PER_PIXEL));
        let slots = Arc::new(Semaphore::new(1));
        let waiting = Arc::new(Semaphore::new(1));
        assert!(matches!(
            RenderJob::acquire_raster_on(
                slots.clone(),
                waiting.clone(),
                Duration::from_secs(2),
                memory.clone(),
                2,
                1,
            )
            .await,
            Err(ExecutionError::Busy)
        ));
        assert_eq!(waiting.available_permits(), 1);
        let held = memory.try_reserve(1, 1).unwrap();
        assert!(matches!(
            RenderJob::acquire_raster_on(
                slots.clone(),
                waiting.clone(),
                Duration::from_millis(10),
                memory.clone(),
                1,
                1,
            )
            .await,
            Err(ExecutionError::Timeout)
        ));
        assert_eq!(memory.rejected(), 2);
        assert_eq!(waiting.available_permits(), 1);
        assert_eq!(slots.available_permits(), 1);
        drop(held);
        // A canceled CPU waiter must also release memory already reserved.
        let cpu = slots.clone().acquire_owned().await.unwrap();
        let task = tokio::spawn(RenderJob::acquire_raster_on(
            slots.clone(),
            waiting.clone(),
            Duration::from_secs(2),
            memory.clone(),
            1,
            1,
        ));
        wait_until_queued(&waiting, 0).await;
        assert_eq!(memory.available(), 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(memory.available(), memory.capacity());
        drop(cpu);
    }

    #[test]
    fn expired_dispatch_is_counted_once_per_request() {
        const CHILD: &str = "MC_TEST_RENDER_DEADLINE_COUNTER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::expired_dispatch_is_counted_once_per_request",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // Process isolation makes the cumulative counter assertion independent
        // of timeout tests running concurrently elsewhere in this test binary.
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let before = metrics().timed_out;
            for completed in 1..=20 {
                let job = RenderJob::acquire_on(
                    Arc::new(Semaphore::new(1)),
                    Arc::new(Semaphore::new(0)),
                    Duration::ZERO,
                )
                .await
                .unwrap();
                assert!(matches!(
                    job.run(|| panic!("expired work ran")).await,
                    Err(ExecutionError::Timeout)
                ));
                assert_eq!(metrics().timed_out, before + completed);
            }
        });
    }

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
        let memory = Arc::new(budget::RenderBudget::new(budget::BYTES_PER_PIXEL));
        let (job, reservation) = RenderJob::acquire_raster_on(
            slots.clone(),
            Arc::new(Semaphore::new(0)),
            Duration::from_millis(100),
            memory.clone(),
            1,
            1,
        )
        .await
        .unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(job.run(move || {
            let _reservation = reservation;
            started.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(ds_core::deadline::check().is_err());
        }));
        started_rx.await.unwrap();
        assert!(matches!(task.await.unwrap(), Err(ExecutionError::Timeout)));
        assert_eq!(slots.available_permits(), 0);
        assert_eq!(memory.available(), 0);
        release.send(()).unwrap();
        let _released = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(memory.available(), memory.capacity());
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
