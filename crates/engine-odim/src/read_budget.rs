//! Shared admission for the transient buffers of PVOL EDR walks (#1041)
//! and of the composite reads an EDR query runs beside its first (#1495),
//! separate from the pixel and composite caches.
use std::sync::{Arc, LazyLock};

pub(crate) use ds_executor::budget::{ByteBudget as Budget, BytePermit as Permit};

/// EDR read budget in bytes, `MC_PVOL_READ_MEMORY_MB` (default 1024 MiB),
/// shared by every polar-volume site and EDR query, WMS GetFeatureInfo's
/// position query included. Charged per volume of a walk
/// (`volume_engine::walk_volumes`) before its file is read: the bytes it
/// fetches (its moments' byte ranges, else the whole file, read ahead or
/// not) until they are decoded, and the arrays its batch decodes, at
/// four bytes a sample, until the walk moves on to the next volume. Moments
/// already resident in the pixel cache cost nothing. A COMP EDR query's
/// composite reads beside its first reserve the file and its decode in
/// the background half (`edr::FrameAdmission`), and wait for the query's
/// unbudgeted read instead of failing when it is full. Map, 3D Tiles and
/// storm-cell reads are not charged. A reservation that does not fit fails
/// at once with `ResourceExhausted`: HTTP 503 in EDR and GetFeatureInfo.
pub(crate) static BUDGET: LazyLock<Arc<Budget>> = LazyLock::new(|| {
    let (bytes, source) = ds_cache::plan::env_bytes("MC_PVOL_READ_MEMORY_MB", 1024);
    let budget = Arc::new(Budget::new(bytes));
    budget.register(
        "pvol_read",
        ds_cache::plan::Scope::Engine("odim-volume"),
        source,
    );
    budget
});

/// (reserved bytes, capacity bytes, rejected admissions).
pub fn metrics() -> (u64, u64, u64) {
    BUDGET.metrics()
}
