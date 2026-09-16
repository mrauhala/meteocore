//! Shared admission for transient GeoTIFF decode buffers, separate from caches.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, LazyLock,
};

use ds_core::error::DataServerError;

pub(crate) static BUDGET: LazyLock<Arc<Budget>> = LazyLock::new(|| {
    let bytes = std::env::var("MC_GEOTIFF_DECODE_MEMORY_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .and_then(|v| v.checked_mul(1024 * 1024))
        .unwrap_or(1024 * 1024 * 1024);
    Arc::new(Budget::new(bytes))
});

pub(crate) struct Budget {
    limit: usize,
    used: AtomicUsize,
    rejected: AtomicUsize,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
        }
    }

    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Permit, DataServerError> {
        if self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&n| n <= self.limit)
            })
            .is_err()
        {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(DataServerError::ResourceExhausted);
        }
        Ok(Permit {
            budget: self.clone(),
            bytes,
        })
    }
}

pub(crate) struct Permit {
    budget: Arc<Budget>,
    bytes: usize,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// (reserved bytes, capacity bytes, rejected admissions).
pub fn metrics() -> (usize, usize, usize) {
    (
        BUDGET.used.load(Ordering::Relaxed),
        BUDGET.limit,
        BUDGET.rejected.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_admission_is_bounded_and_unwind_releases_bytes() {
        let budget = Arc::new(Budget::new(100));
        let barrier = Arc::new(std::sync::Barrier::new(20));
        std::thread::scope(|scope| {
            for _ in 0..20 {
                let budget = budget.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let permit = budget.reserve(10);
                    barrier.wait();
                    assert_eq!(budget.used.load(Ordering::Relaxed), 100);
                    barrier.wait();
                    drop(permit);
                });
            }
        });
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
        assert_eq!(budget.rejected.load(Ordering::Relaxed), 10);
        assert!(budget.reserve(usize::MAX).is_err());
        let _ = std::panic::catch_unwind(|| {
            let _permit = budget.reserve(100).unwrap();
            panic!("decoder failed");
        });
        assert!(budget.reserve(100).is_ok());
    }
}
