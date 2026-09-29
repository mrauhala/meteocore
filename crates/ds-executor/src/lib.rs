//! Bounded render admission and blocking execution shared by HTTP APIs.
pub mod budget;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Built-in slot count: 2× available CPUs, min 8. A slot's "ownership" of a
/// CPU is loose because decode/encode interleaves with bilinear passes.
pub fn default_render_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .saturating_mul(2)
        .max(8)
}

static SLOT_COUNT: OnceLock<usize> = OnceLock::new();

/// The process-wide render slot count. The first read fixes it: the value
/// from [`set_render_concurrency`], else [`default_render_concurrency`].
pub fn render_concurrency() -> usize {
    *SLOT_COUNT.get_or_init(default_render_concurrency)
}

/// Fix the slot count from `[server] render_concurrency` (#209; range is
/// validated at config load). Call before anything reads the slots: they are
/// sized once and survive reloads. Too late, it returns the fixed count.
pub fn set_render_concurrency(slots: usize) -> Result<(), usize> {
    match SLOT_COUNT.set(slots) {
        Ok(()) => Ok(()),
        Err(_) if render_concurrency() == slots => Ok(()),
        Err(_) => Err(render_concurrency()),
    }
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
/// Deadline expiries by stage: waiting for memory or a slot, or admitted.
static TIMED_OUT_QUEUE: AtomicU64 = AtomicU64::new(0);
static TIMED_OUT_RENDER: AtomicU64 = AtomicU64::new(0);

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
    /// Shed at once because the waiting queue was full: never a deadline.
    pub rejected: u64,
    /// Deadline expired while waiting for raster memory or a render slot.
    pub timed_out_queue: u64,
    /// Deadline expired after admission: blocking-pool dispatch, engine read
    /// or encoding (#147).
    pub timed_out_render: u64,
}
pub fn metrics() -> Metrics {
    Metrics {
        queued: QUEUE_CAPACITY.saturating_sub(WAITING.available_permits()),
        capacity: *QUEUE_CAPACITY,
        rejected: REJECTED.load(Ordering::Relaxed),
        timed_out_queue: TIMED_OUT_QUEUE.load(Ordering::Relaxed),
        timed_out_render: TIMED_OUT_RENDER.load(Ordering::Relaxed),
    }
}

/// Which path served a raster render response: the fixed `outcome` label of
/// the server's `render_duration_seconds` histogram (#466). Kept apart so
/// sub-millisecond hits no longer bury the cold-render tail (#248).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderOutcome {
    /// Served from the rendered-output cache, without admission.
    Hit,
    /// A WMS view assembled from meta-tiles that were all cached: no engine read.
    Assembled,
    /// The engine read source data: a direct render, or a meta-tiled view
    /// with at least one uncached tile.
    Cold,
}

impl RenderOutcome {
    pub const ALL: [Self; 3] = [Self::Hit, Self::Assembled, Self::Cold];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Assembled => "assembled",
            Self::Cold => "cold",
        }
    }
}

/// One step of a served render: the fixed `phase` label of the server's
/// `render_phase_seconds` histogram (#147).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPhase {
    /// Admission: waiting for raster memory and a render slot.
    Queue,
    /// The engine's `get_raster_tile`: source read, decode and reproject.
    /// Summed over the uncached tiles of a meta-tiled WMS view.
    Engine,
    /// WMS meta-tiling only: resampling cached tiles into the viewport.
    Assemble,
    /// Colorize and image encode.
    Encode,
}

impl RenderPhase {
    pub const ALL: [Self; 4] = [Self::Queue, Self::Engine, Self::Assemble, Self::Encode];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Engine => "engine",
            Self::Assemble => "assemble",
            Self::Encode => "encode",
        }
    }
}

/// Where one served render spent its time, by [`RenderPhase`]. A phase the
/// render skipped (a hit's admission, an all-nodata tile's encode) stays
/// unset and records nothing, so it cannot drag that phase's quantiles to 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderPhases([Option<Duration>; 4]);

impl RenderPhases {
    /// Add `elapsed` to `phase`.
    pub fn add(&mut self, phase: RenderPhase, elapsed: Duration) {
        let slot = &mut self.0[phase as usize];
        *slot = Some(slot.unwrap_or_default() + elapsed);
    }

    pub fn get(&self, phase: RenderPhase) -> Option<Duration> {
        self.0[phase as usize]
    }

    /// The phases this render ran, in [`RenderPhase::ALL`] order.
    pub fn iter(&self) -> impl Iterator<Item = (RenderPhase, Duration)> + '_ {
        RenderPhase::ALL
            .into_iter()
            .filter_map(|phase| self.get(phase).map(|elapsed| (phase, elapsed)))
    }
}

/// Response extension carrying one render's latency, from the rendered-cache
/// lookup to the response, and its phase breakdown. The raster API handlers
/// attach it and the server's metrics middleware records it, so engines and
/// API crates stay metric-free. Only served renders carry one: shed,
/// timed-out and failed renders have their own counters and error responses.
#[derive(Clone, Debug)]
pub struct RenderTiming {
    /// The collection's registry id: config-bounded, never a raw layer name.
    pub collection: String,
    pub outcome: RenderOutcome,
    pub elapsed: Duration,
    /// Empty for a hit, which runs none of the phases.
    pub phases: RenderPhases,
}

impl RenderTiming {
    /// The latency of a render block that began at `start`.
    pub fn since(collection: &str, outcome: RenderOutcome, start: Instant) -> Self {
        Self {
            collection: collection.to_owned(),
            outcome,
            elapsed: start.elapsed(),
            phases: RenderPhases::default(),
        }
    }

    /// The same timing, with the phases the render measured.
    pub fn with_phases(self, phases: RenderPhases) -> Self {
        Self { phases, ..self }
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
        Self::acquire_raster_planes(slots, width, height, 1).await
    }

    /// [`Self::acquire_raster`] for a render that holds `planes` value
    /// planes of the output size at once: an RGB composite's bands (#819).
    /// It charges [`budget::raster_bytes`], the single-plane charge plus
    /// [`budget::PLANE_BYTES_PER_PIXEL`] per further plane, against the same
    /// memory budget, queue and deadline.
    pub async fn acquire_raster_planes(
        slots: Arc<Semaphore>,
        width: u32,
        height: u32,
        planes: usize,
    ) -> Result<(Self, Arc<budget::RenderPermit>), ExecutionError> {
        Self::acquire_bytes_on(
            slots,
            WAITING.clone(),
            *TIMEOUT,
            budget::RENDER_MEMORY.clone(),
            budget::raster_bytes(width, height, planes),
        )
        .await
    }

    #[cfg(test)]
    async fn acquire_raster_on(
        slots: Arc<Semaphore>,
        waiting: Arc<Semaphore>,
        timeout: Duration,
        memory: Arc<budget::RenderBudget>,
        width: u32,
        height: u32,
    ) -> Result<(Self, Arc<budget::RenderPermit>), ExecutionError> {
        Self::acquire_bytes_on(
            slots,
            waiting,
            timeout,
            memory,
            budget::raster_bytes(width, height, 1),
        )
        .await
    }

    /// Reserve `bytes` of output memory (from [`budget::raster_bytes`];
    /// `None` never fits), then a CPU slot.
    async fn acquire_bytes_on(
        slots: Arc<Semaphore>,
        waiting: Arc<Semaphore>,
        timeout: Duration,
        memory: Arc<budget::RenderBudget>,
        bytes: Option<u64>,
    ) -> Result<(Self, Arc<budget::RenderPermit>), ExecutionError> {
        let deadline = Instant::now() + timeout;
        // A request that can never fit must not occupy the waiting queue.
        if !memory.fits(bytes) {
            memory.reject_oversize();
            return Err(ExecutionError::Busy);
        }
        if let Some(reservation) = memory.try_reserve(bytes) {
            if let Ok(permit) = slots.clone().try_acquire_owned() {
                return Ok((Self { permit, deadline }, Arc::new(reservation)));
            }
        }
        let _queued = waiting.try_acquire_owned().map_err(|_| {
            REJECTED.fetch_add(1, Ordering::Relaxed);
            ExecutionError::Busy
        })?;
        let reservation = tokio::time::timeout_at(deadline.into(), memory.reserve(bytes))
            .await
            .map_err(|_| {
                memory.reject_deadline();
                timeout_error(&TIMED_OUT_QUEUE)
            })?;
        let permit = tokio::time::timeout_at(deadline.into(), slots.acquire_owned())
            .await
            .map_err(|_| timeout_error(&TIMED_OUT_QUEUE))?
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
                    .map_err(|_| timeout_error(&TIMED_OUT_QUEUE))?
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
            .map_err(|_| timeout_error(&TIMED_OUT_RENDER))??;
        // Count at the request boundary, exactly once. Counting inside the
        // worker would race the outer timeout and could count one expiry twice.
        if matches!(&result, Err(ExecutionError::Timeout)) || Instant::now() >= deadline {
            return Err(timeout_error(&TIMED_OUT_RENDER));
        }
        result
    }
}

/// Count one deadline expiry against its stage's counter.
fn timeout_error(stage: &AtomicU64) -> ExecutionError {
    stage.fetch_add(1, Ordering::Relaxed);
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
        let held = memory.try_reserve(budget::raster_bytes(1, 1, 1)).unwrap();
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
        // Shed at once, never queued: attributed apart from deadline expiries.
        assert_eq!(
            (memory.rejected_oversize(), memory.rejected_deadline()),
            (1, 0)
        );
        let held = memory.try_reserve(budget::raster_bytes(1, 1, 1)).unwrap();
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
        assert_eq!(
            (memory.rejected_oversize(), memory.rejected_deadline()),
            (1, 1)
        );
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
    fn deadline_expiries_are_counted_once_by_stage() {
        const CHILD: &str = "MC_TEST_RENDER_DEADLINE_COUNTER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::deadline_expiries_are_counted_once_by_stage",
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
            // Expired waiting for a slot: the queue stage.
            assert!(matches!(
                RenderJob::acquire_on(
                    Arc::new(Semaphore::new(0)),
                    Arc::new(Semaphore::new(1)),
                    Duration::from_millis(10),
                )
                .await,
                Err(ExecutionError::Timeout)
            ));
            assert_eq!(
                (metrics().timed_out_queue, metrics().timed_out_render),
                (1, 0)
            );
            // Admitted, then expired before dispatch: the render stage.
            let before = metrics().timed_out_render;
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
                assert_eq!(metrics().timed_out_render, before + completed);
            }
            assert_eq!(metrics().timed_out_queue, 1);
        });
    }

    #[test]
    fn render_phases_record_only_the_phases_that_ran() {
        let mut phases = RenderPhases::default();
        assert_eq!(phases.iter().count(), 0, "a hit runs no phase");
        phases.add(RenderPhase::Encode, Duration::from_millis(2));
        phases.add(RenderPhase::Queue, Duration::from_millis(1));
        phases.add(RenderPhase::Engine, Duration::ZERO);
        // A meta-tiled view adds colorize and the final encode to one phase.
        phases.add(RenderPhase::Encode, Duration::from_millis(3));
        let recorded: Vec<_> = phases.iter().map(|(phase, _)| phase.as_str()).collect();
        assert_eq!(recorded, ["queue", "engine", "encode"]);
        assert_eq!(phases.get(RenderPhase::Assemble), None);
        assert_eq!(
            phases.get(RenderPhase::Encode),
            Some(Duration::from_millis(5))
        );
        let timing = RenderTiming::since("radar", RenderOutcome::Cold, Instant::now());
        assert_eq!(timing.phases, RenderPhases::default());
        assert_eq!(timing.with_phases(phases).phases, phases);
    }

    /// Re-runs `test` alone in a child process and returns `true` in the
    /// parent: the slot count is process-global and fixed by its first read.
    fn ran_in_child(test: &str) -> bool {
        const CHILD: &str = "MC_TEST_RENDER_CONCURRENCY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return false;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .env_remove("MC_RENDER_QUEUE_CAPACITY")
            .status()
            .unwrap();
        assert!(status.success(), "{test} failed in its child process");
        true
    }

    #[test]
    fn configured_render_concurrency_sizes_slots_and_queue() {
        if ran_in_child("tests::configured_render_concurrency_sizes_slots_and_queue") {
            return;
        }
        assert_eq!(set_render_concurrency(3), Ok(()));
        assert_eq!(
            set_render_concurrency(3),
            Ok(()),
            "same value is idempotent"
        );
        assert_eq!(render_concurrency(), 3);
        assert_eq!(RENDER_SLOTS.available_permits(), 3);
        assert_eq!(metrics().capacity, 9, "queue defaults to 3x the slots");
        assert_eq!(set_render_concurrency(4), Err(3));
        assert_eq!(RENDER_SLOTS.available_permits(), 3);
    }

    #[test]
    fn render_concurrency_is_fixed_by_its_first_read() {
        if ran_in_child("tests::render_concurrency_is_fixed_by_its_first_read") {
            return;
        }
        let default = default_render_concurrency();
        assert_eq!(RENDER_SLOTS.available_permits(), default);
        assert_eq!(set_render_concurrency(default + 1), Err(default));
        assert_eq!(render_concurrency(), default);
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
