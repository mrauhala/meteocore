//! Shared admission for transient GeoTIFF decode buffers, separate from caches.
use std::sync::{Arc, LazyLock};

use ds_cache::plan::{Scope, SizeSource};
pub(crate) use ds_executor::budget::{ByteBudget as Budget, BytePermit as Permit};

/// Source-decode budget in bytes, `MC_GEOTIFF_DECODE_MEMORY_MB` (default
/// 1024 MiB), shared by every GeoTIFF collection and API. Charged per source
/// tile fetched or decoded, not per output pixel; separate from both the
/// source-pixel cap `reader::MAX_MAP_PIXELS` and render output admission. A
/// reservation that does not fit fails at once with `ResourceExhausted`:
/// HTTP 503 in WMS/Maps/Tiles/EDR.
pub(crate) static BUDGET: LazyLock<Arc<Budget>> = LazyLock::new(|| {
    const VAR: &str = "MC_GEOTIFF_DECODE_MEMORY_MB";
    let set = std::env::var(VAR)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .and_then(|v| v.checked_mul(1024 * 1024));
    let source = match set {
        Some(_) => SizeSource::Env(VAR),
        None => SizeSource::Default,
    };
    let budget = Arc::new(Budget::new(set.unwrap_or(1024 * 1024 * 1024) as u64));
    budget.register("geotiff_decode", Scope::Engine("geotiff"), source);
    budget
});

/// (reserved bytes, capacity bytes, rejected admissions).
pub fn metrics() -> (u64, u64, u64) {
    BUDGET.metrics()
}
