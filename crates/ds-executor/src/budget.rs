//! Process-wide admission accounting for transient raster render buffers, and
//! the fail-fast byte budget engines charge their source reads against.
//!
//! Both are separate from cache capacity. [`RenderBudget`] admits render
//! output and waits for memory within the render deadline. Its estimate
//! covers a boxed f64 raster, RGBA, and encoding/assembly scratch (32
//! bytes/output pixel), rather than counting only the final four-byte RGBA
//! buffer. [`ByteBudget`] admits an engine's transient source buffers and
//! never waits.
use ds_core::error::DataServerError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::Notify;

/// Bytes charged per *output* pixel (`width × height` of the rendered image,
/// never source pixels) against [`RENDER_MEMORY`].
pub const BYTES_PER_PIXEL: u64 = 32;

/// Extra bytes per output pixel for each value plane a render holds beyond
/// the first: one boxed `Option<f64>` sample, the widest plane an engine
/// returns. An RGB composite holds one plane per band (#819).
pub const PLANE_BYTES_PER_PIXEL: u64 = 16;

/// Bytes a raster render of `width × height` output pixels charges while it
/// holds `planes` value planes at once: [`BYTES_PER_PIXEL`], which covers
/// one plane, the RGBA output and encoding scratch, plus
/// [`PLANE_BYTES_PER_PIXEL`] per further plane. `planes` of 0 counts as 1.
/// `None` when the product overflows, which never fits.
pub fn raster_bytes(width: u32, height: u32, planes: usize) -> Option<u64> {
    let extra = u64::try_from(planes.saturating_sub(1))
        .ok()?
        .checked_mul(PLANE_BYTES_PER_PIXEL)?;
    u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(BYTES_PER_PIXEL.checked_add(extra)?)
}

/// An environment setting, like the other process-wide render/cache budgets.
/// A single instance survives collection reloads and spans all raster APIs.
///
/// Output-allocation budget in bytes: `MC_RENDER_MEMORY_MB` (default 1024 MiB,
/// i.e. 33 554 432 output pixels at [`BYTES_PER_PIXEL`]), charged by
/// `RenderJob::acquire_raster` for every WMS/Maps/Tiles cache miss, and by
/// `RenderJob::acquire_raster_planes` with [`raster_bytes`] for a render that
/// holds several value planes (an RGB composite's bands). At the
/// default it is tighter than the APIs' own 64 M-pixel `MAX_MAP_PIXELS`.
/// Temporary exhaustion waits in the bounded queue, at most until the render
/// deadline;
/// a request larger than the whole budget is rejected at once as
/// `ExecutionError::Busy`. Both reach the client as HTTP 503 "Server busy, try
/// again later" with `Retry-After: 1`, although retrying an oversized request
/// cannot succeed ("Pixel budgets" in the root CLAUDE.md, #120).
pub static RENDER_MEMORY: LazyLock<Arc<RenderBudget>> = LazyLock::new(|| {
    Arc::new(RenderBudget::new(
        ds_cache::env_mb("MC_RENDER_MEMORY_MB", 1024).saturating_mul(1024 * 1024),
    ))
});

/// Waiting admission for render output buffers: a charge that does not fit
/// yet waits for releases until the render deadline. Distinct from
/// [`ByteBudget`], the engines' source-read budget, which never waits.
#[derive(Debug)]
pub struct RenderBudget {
    capacity: u64,
    used: AtomicU64,
    /// Requests larger than the whole budget, shed before queueing.
    oversize: AtomicU64,
    /// Requests whose deadline expired while waiting for memory.
    expired: AtomicU64,
    released: Notify,
}

impl RenderBudget {
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity,
            used: AtomicU64::new(0),
            oversize: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            released: Notify::new(),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }
    pub fn available(&self) -> u64 {
        self.capacity
            .saturating_sub(self.used.load(Ordering::Relaxed))
    }
    /// Every rejection: [`Self::rejected_oversize`] plus [`Self::rejected_deadline`].
    pub fn rejected(&self) -> u64 {
        self.rejected_oversize() + self.rejected_deadline()
    }
    /// Requests larger than the whole budget: an immediate 503 that retrying
    /// cannot fix, never a wait.
    pub fn rejected_oversize(&self) -> u64 {
        self.oversize.load(Ordering::Relaxed)
    }
    /// Requests whose deadline expired while waiting for memory; each is also
    /// a queue-stage render deadline expiry.
    pub fn rejected_deadline(&self) -> u64 {
        self.expired.load(Ordering::Relaxed)
    }

    /// Whether a charge of `bytes` (from [`raster_bytes`]) can ever fit.
    pub(crate) fn fits(&self, bytes: Option<u64>) -> bool {
        bytes.is_some_and(|bytes| bytes <= self.capacity)
    }

    pub(crate) fn reject_oversize(&self) {
        self.oversize.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reject_deadline(&self) {
        self.expired.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) async fn reserve(self: &Arc<Self>, bytes: Option<u64>) -> RenderPermit {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            // Register before checking availability so a concurrent release cannot be lost.
            released.as_mut().enable();
            if let Some(permit) = self.try_reserve(bytes) {
                return permit;
            }
            released.await;
        }
    }

    pub(crate) fn try_reserve(self: &Arc<Self>, bytes: Option<u64>) -> Option<RenderPermit> {
        if let Some(bytes) = bytes {
            if self
                .used
                .try_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                    used.checked_add(bytes)
                        .filter(|&total| total <= self.capacity)
                })
                .is_ok()
            {
                return Some(RenderPermit {
                    budget: self.clone(),
                    bytes,
                });
            }
        }
        None
    }
}

#[derive(Debug)]
pub struct RenderPermit {
    budget: Arc<RenderBudget>,
    bytes: u64,
}

impl Drop for RenderPermit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.released.notify_waiters();
    }
}

/// Fail-fast admission for an engine's transient source buffers: the native
/// reads, decode outputs and conversion copies it holds while it answers a
/// request. Each engine owns one process-wide instance behind its own
/// environment variable and static (engine-geotiff's
/// `MC_GEOTIFF_DECODE_MEMORY_MB`, engine-zarr's `MC_ZARR_READ_MEMORY_MB`);
/// this type only keeps the account. It is not an allocator-enforced limit.
///
/// Distinct from [`RenderBudget`], which counts output pixels and waits in
/// the render queue until the deadline. A reservation here that does not fit
/// fails at once with `ResourceExhausted` (HTTP 503): its callers already
/// hold an executor slot and may hold another reservation, so waiting could
/// deadlock them. Nothing here checks the request deadline; a caller that
/// must, checks before reserving.
#[derive(Debug)]
pub struct ByteBudget {
    capacity: u64,
    used: AtomicU64,
    /// Request admissions turned away. Probes and background refusals are
    /// never counted.
    rejected: AtomicU64,
}

impl ByteBudget {
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity,
            used: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// (reserved bytes, capacity bytes, rejected admissions), the triple each
    /// engine exports to `/metrics`.
    pub fn metrics(&self) -> (u64, u64, u64) {
        (
            self.used.load(Ordering::Relaxed),
            self.capacity,
            self.rejected.load(Ordering::Relaxed),
        )
    }

    /// Reserve `bytes` for a request, or count a rejected admission and fail
    /// with `ResourceExhausted` at once.
    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Result<BytePermit, DataServerError> {
        self.try_reserve(bytes).ok_or_else(|| self.reject())
    }

    /// Reserve `bytes` if they fit now, without counting a refusal: a probe
    /// for one more slot of a batch, whose smaller fan-out is not a rejected
    /// request. The caller must [`Self::reject`] if even its first slot
    /// cannot fit.
    pub fn try_reserve(self: &Arc<Self>, bytes: u64) -> Option<BytePermit> {
        self.admit(bytes, self.capacity)
    }

    /// Count one rejected admission and return its error, for a caller whose
    /// own estimate shows the request cannot fit.
    pub fn reject(&self) -> DataServerError {
        self.rejected.fetch_add(1, Ordering::Relaxed);
        DataServerError::ResourceExhausted
    }

    /// The most background work may hold, together with every request:
    /// half the budget. A background reservation larger than this never fits.
    pub fn background_limit(&self) -> u64 {
        self.capacity / 2
    }

    /// Admission for background work, such as engine-geotiff's poll-cycle
    /// pre-warm (#1004): it fits only while at least half the budget stays
    /// free for requests, and a refusal is not counted as a rejected
    /// admission, which tracks requests turned away with 503. `None` means
    /// not now: requests hold too much, or `bytes` exceeds
    /// [`Self::background_limit`].
    pub fn try_reserve_background(self: &Arc<Self>, bytes: u64) -> Option<BytePermit> {
        self.admit(bytes, self.background_limit())
    }

    fn admit(self: &Arc<Self>, bytes: u64, limit: u64) -> Option<BytePermit> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&n| n <= limit)
            })
            .ok()?;
        Some(BytePermit {
            budget: self.clone(),
            bytes,
        })
    }
}

/// Bytes held from a [`ByteBudget`], released when the permit drops,
/// including on unwind.
#[derive(Debug)]
pub struct BytePermit {
    budget: Arc<ByteBudget>,
    bytes: u64,
}

impl Drop for BytePermit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_size_admission_and_release() {
        let budget = Arc::new(RenderBudget::new(100 * BYTES_PER_PIXEL));
        let small = budget.try_reserve(raster_bytes(2, 5, 1)).unwrap();
        let large = budget.try_reserve(raster_bytes(9, 10, 1)).unwrap();
        assert_eq!(budget.available(), 0);
        assert!(budget.try_reserve(raster_bytes(1, 1, 1)).is_none());
        drop(small);
        assert_eq!(budget.available(), 10 * BYTES_PER_PIXEL);
        assert!(budget.try_reserve(raster_bytes(11, 10, 1)).is_none());
        drop(large);
        assert_eq!(budget.available(), budget.capacity());
        assert!(budget
            .try_reserve(raster_bytes(u32::MAX, u32::MAX, 1))
            .is_none());
    }

    /// Each value plane past the first adds `PLANE_BYTES_PER_PIXEL`: a
    /// three-band composite charges twice a single-band render.
    #[test]
    fn planes_add_to_the_single_plane_charge() {
        assert_eq!(raster_bytes(10, 10, 1), Some(100 * BYTES_PER_PIXEL));
        assert_eq!(raster_bytes(10, 10, 0), raster_bytes(10, 10, 1));
        assert_eq!(raster_bytes(10, 10, 3), Some(200 * BYTES_PER_PIXEL));
        assert_eq!(
            raster_bytes(10, 10, 4),
            Some(100 * (BYTES_PER_PIXEL + 3 * PLANE_BYTES_PER_PIXEL))
        );
        assert_eq!(raster_bytes(u32::MAX, u32::MAX, 2), None);
        let budget = Arc::new(RenderBudget::new(2 * BYTES_PER_PIXEL));
        assert!(budget.fits(raster_bytes(1, 1, 3)));
        assert!(!budget.fits(raster_bytes(1, 1, 4)));
        let composite = budget.try_reserve(raster_bytes(1, 1, 3)).unwrap();
        assert!(budget.try_reserve(raster_bytes(1, 1, 1)).is_none());
        drop(composite);
        assert_eq!(budget.available(), budget.capacity());
    }

    #[test]
    fn worker_retains_reservation_after_request_is_dropped() {
        let budget = Arc::new(RenderBudget::new(BYTES_PER_PIXEL));
        let request = Arc::new(budget.try_reserve(raster_bytes(1, 1, 1)).unwrap());
        let worker = request.clone();
        drop(request);
        assert!(budget.try_reserve(raster_bytes(1, 1, 1)).is_none());
        drop(worker);
        assert!(budget.try_reserve(raster_bytes(1, 1, 1)).is_some());
    }

    #[test]
    fn concurrent_admission_cannot_overcommit() {
        let budget = Arc::new(RenderBudget::new(4 * BYTES_PER_PIXEL));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        std::thread::scope(|scope| {
            let threads: Vec<_> = (0..16)
                .map(|_| {
                    let budget = budget.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        let permit = budget.try_reserve(raster_bytes(1, 1, 1));
                        barrier.wait();
                        permit
                    })
                })
                .collect();
            let permits: Vec<_> = threads
                .into_iter()
                .filter_map(|t| t.join().unwrap())
                .collect();
            assert_eq!(permits.len(), 4);
            assert_eq!(budget.rejected(), 0);
        });
        assert_eq!(budget.available(), budget.capacity());
    }

    #[test]
    fn byte_reserve_releases_on_drop_and_counts_each_rejection() {
        let budget = Arc::new(ByteBudget::new(100));
        let first = budget.reserve(60).unwrap();
        let second = budget.reserve(40).unwrap();
        assert_eq!(budget.metrics(), (100, 100, 0));
        assert!(matches!(
            budget.reserve(1),
            Err(DataServerError::ResourceExhausted)
        ));
        assert_eq!(budget.metrics(), (100, 100, 1));
        drop(first);
        assert_eq!(budget.metrics(), (40, 100, 1));
        assert!(budget.reserve(61).is_err());
        assert_eq!(budget.metrics(), (40, 100, 2));
        let rest = budget.reserve(60).unwrap();
        drop((second, rest));
        assert_eq!(budget.metrics(), (0, 100, 2));
        // Overflowing the running total never fits.
        let held = budget.reserve(1).unwrap();
        assert!(budget.reserve(u64::MAX).is_err());
        drop(held);
        assert!(matches!(
            budget.reject(),
            DataServerError::ResourceExhausted
        ));
        assert_eq!(budget.metrics(), (0, 100, 4));
        // A zero budget still admits nothing larger than zero bytes.
        let zero = Arc::new(ByteBudget::new(0));
        assert!(zero.reserve(0).is_ok());
        assert!(zero.reserve(1).is_err());
        assert_eq!(zero.metrics(), (0, 0, 1));
    }

    /// A batch probing for one more slot is not a rejected request.
    #[test]
    fn byte_probe_never_counts_a_refusal() {
        let budget = Arc::new(ByteBudget::new(100));
        let held = budget.try_reserve(60).unwrap();
        assert!(budget.try_reserve(60).is_none());
        assert!(budget.try_reserve(u64::MAX).is_none());
        assert_eq!(budget.metrics(), (60, 100, 0));
        let rest = budget.try_reserve(40).unwrap();
        assert_eq!(budget.metrics(), (100, 100, 0));
        drop((held, rest));
        assert_eq!(budget.metrics(), (0, 100, 0));
    }

    #[test]
    fn concurrent_byte_admission_is_bounded_and_unwind_releases_bytes() {
        let budget = Arc::new(ByteBudget::new(100));
        let barrier = Arc::new(std::sync::Barrier::new(20));
        std::thread::scope(|scope| {
            for _ in 0..20 {
                let budget = budget.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let permit = budget.reserve(10);
                    barrier.wait();
                    assert_eq!(budget.metrics().0, 100);
                    barrier.wait();
                    drop(permit);
                });
            }
        });
        assert_eq!(budget.metrics(), (0, 100, 10));
        let _ = std::panic::catch_unwind(|| {
            let _permit = budget.reserve(100).unwrap();
            panic!("decoder failed");
        });
        assert!(budget.reserve(100).is_ok());
    }

    /// #1004: background work never takes the second half of the budget
    /// and is never counted as a rejected request.
    #[test]
    fn background_admission_leaves_half_for_requests() {
        let budget = Arc::new(ByteBudget::new(100));
        assert_eq!(budget.background_limit(), 50);
        let request = budget.reserve(30).unwrap();
        let background = budget.try_reserve_background(20).expect("50 of 100 fits");
        assert!(budget.try_reserve_background(1).is_none(), "past half");
        assert_eq!(budget.metrics(), (50, 100, 0));
        // Requests may still use the rest.
        let rest = budget.reserve(50).unwrap();
        drop((request, background, rest));
        assert_eq!(budget.metrics(), (0, 100, 0));
        // Larger than half never fits, even on an idle budget.
        assert!(budget.try_reserve_background(51).is_none());
        assert_eq!(budget.metrics(), (0, 100, 0));
        assert!(Arc::new(ByteBudget::new(0))
            .try_reserve_background(1)
            .is_none());
        // An odd capacity rounds the background half down.
        assert_eq!(ByteBudget::new(101).background_limit(), 50);
    }
}
