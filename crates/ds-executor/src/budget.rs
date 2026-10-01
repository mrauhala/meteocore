//! Process-wide admission accounting for transient raster render buffers.
//!
//! Separate from cache capacity and source decode budgets. The estimate covers
//! a boxed f64 raster, RGBA, and encoding/assembly scratch (32 bytes/output
//! pixel), rather than counting only the final four-byte RGBA buffer.
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

    // `fetch_update` is deprecated since Rust 1.99 in favour of `try_update`,
    // which is still unstable on the Docker image's Rust 1.94: switch once
    // the image moves to 1.98 or later.
    #[allow(deprecated)]
    pub(crate) fn try_reserve(self: &Arc<Self>, bytes: Option<u64>) -> Option<RenderPermit> {
        if let Some(bytes) = bytes {
            if self
                .used
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
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
}
