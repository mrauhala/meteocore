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
    let (bytes, source) = size("MC_GEOTIFF_DECODE_MEMORY_MB");
    let budget = Arc::new(Budget::new(bytes));
    budget.register("geotiff_decode", Scope::Engine("geotiff"), source);
    budget
});

/// The budget's size from `var`, in MiB, and where it came from: as every
/// `MC_…` size, a value that is not a whole number keeps the 1024 MiB
/// default but pins it, which the plan log warns about
/// ([`ds_cache::plan::env_size`]).
fn size(var: &'static str) -> (u64, SizeSource) {
    ds_cache::plan::env_bytes(var, 1024)
}

/// (reserved bytes, capacity bytes, rejected admissions).
pub fn metrics() -> (u64, u64, u64) {
    BUDGET.metrics()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1312: a size that is not a whole number pins the default, as every
    /// `MC_…` cache size does; a whole number, trimmed, sizes it.
    #[test]
    fn an_unparsable_size_pins_the_default() {
        const VAR: &str = "MC_TEST_GEOTIFF_DECODE_MEMORY_MB";
        assert_eq!(size(VAR), (1024 << 20, SizeSource::Default));
        std::env::set_var(VAR, "2G");
        assert_eq!(size(VAR), (1024 << 20, SizeSource::Env(VAR)));
        std::env::set_var(VAR, " 512 ");
        assert_eq!(size(VAR), (512 << 20, SizeSource::Env(VAR)));
    }
}
