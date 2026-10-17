//! Shared admission for the transient buffers of NetCDF field reads,
//! separate from the field cache.
use std::sync::{Arc, LazyLock};

pub(crate) use ds_executor::budget::ByteBudget as Budget;

/// Field-read budget in bytes, `MC_NETCDF_READ_MEMORY_MB` (default 1024
/// MiB), shared by every NetCDF collection. Charged by a field's fill owner
/// for the read's transient peak ([`crate::field`]); cache hits and readers
/// waiting for another's fill charge nothing. A reservation that does not
/// fit fails at once with `ResourceExhausted`, HTTP 503: the reader holds a
/// render slot, and waiting could deadlock it.
pub(crate) static BUDGET: LazyLock<Arc<Budget>> = LazyLock::new(|| {
    Arc::new(Budget::new(
        ds_cache::env_mb("MC_NETCDF_READ_MEMORY_MB", 1024).saturating_mul(ds_cache::MIB),
    ))
});

/// (reserved bytes, capacity bytes, rejected admissions).
pub fn metrics() -> (u64, u64, u64) {
    BUDGET.metrics()
}
