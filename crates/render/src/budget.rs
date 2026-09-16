//! Process-wide admission accounting for transient raster render buffers.
//!
//! Separate from cache capacity and source decode budgets. The estimate covers
//! a boxed f64 raster, RGBA, and encoding/assembly scratch (32 bytes/output
//! pixel), rather than counting only the final four-byte RGBA buffer.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

pub const BYTES_PER_PIXEL: u64 = 32;

/// An environment setting, like the other process-wide render/cache budgets.
/// A single instance survives collection reloads and spans all raster APIs.
pub static RENDER_MEMORY: LazyLock<Arc<RenderBudget>> = LazyLock::new(|| {
    Arc::new(RenderBudget::new(
        ds_cache::env_mb("MC_RENDER_MEMORY_MB", 1024).saturating_mul(1024 * 1024),
    ))
});

pub struct RenderBudget {
    capacity: u64,
    used: AtomicU64,
    rejected: AtomicU64,
}

impl RenderBudget {
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
    pub fn available(&self) -> u64 {
        self.capacity
            .saturating_sub(self.used.load(Ordering::Relaxed))
    }
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }

    pub fn try_acquire(self: &Arc<Self>, width: u32, height: u32) -> Option<RenderPermit> {
        let bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL));
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
        self.rejected.fetch_add(1, Ordering::Relaxed);
        None
    }
}

pub struct RenderPermit {
    budget: Arc<RenderBudget>,
    bytes: u64,
}

impl Drop for RenderPermit {
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
        let small = budget.try_acquire(2, 5).unwrap();
        let large = budget.try_acquire(9, 10).unwrap();
        assert_eq!(budget.available(), 0);
        assert!(budget.try_acquire(1, 1).is_none());
        drop(small);
        assert_eq!(budget.available(), 10 * BYTES_PER_PIXEL);
        assert!(budget.try_acquire(11, 10).is_none());
        drop(large);
        assert_eq!(budget.available(), budget.capacity());
        assert!(budget.try_acquire(u32::MAX, u32::MAX).is_none());
    }

    #[test]
    fn worker_retains_reservation_after_request_is_dropped() {
        let budget = Arc::new(RenderBudget::new(BYTES_PER_PIXEL));
        let request = Arc::new(budget.try_acquire(1, 1).unwrap());
        let worker = request.clone();
        drop(request);
        assert!(budget.try_acquire(1, 1).is_none());
        drop(worker);
        assert!(budget.try_acquire(1, 1).is_some());
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
                        let permit = budget.try_acquire(1, 1);
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
            assert_eq!(budget.rejected(), 12);
        });
        assert_eq!(budget.available(), budget.capacity());
    }
}
