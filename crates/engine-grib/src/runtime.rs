//! Shared fallback for synchronous callers outside the server's dedicated
//! runtimes, and how many field jobs one EDR query runs at once (#1297).

use std::sync::LazyLock;

use ds_core::error::DataServerError;

/// Field jobs of one EDR query without a published grid geometry: the
/// shared scheduler's default, as before #1297.
pub(crate) use ds_executor::field_jobs::FIELD_CONCURRENCY;

/// The most field jobs one GRIB EDR query runs at once (#1297). A remote
/// field costs a ranged GET of about 0.2–0.3 s, mostly waiting, plus a
/// decode; four at a time made a 300-field GFS window take about 20 s.
pub(crate) const MAX_FIELD_JOBS: usize = 16;

/// Decoded values the field jobs of one query may hold at once, 256 MiB,
/// the default decoded-grid cache: each job holds one decoded `f32` field
/// (and, while decoding, its packed values and compressed message).
/// [`field_concurrency`] divides it by the grid's size, so a 0.25° global
/// field (1440 × 721, 4.2 MB) runs [`MAX_FIELD_JOBS`] and a 0.1° one
/// (3600 × 1801, 26 MB) ten.
pub(crate) const FIELD_MEMORY: u64 = 256 * ds_cache::MIB;

/// How many field jobs one query over a grid of `nodes` values runs at
/// once: [`FIELD_MEMORY`] over one decoded field, between the default
/// [`FIELD_CONCURRENCY`] and [`MAX_FIELD_JOBS`]. Without a published
/// geometry (`None`) the default, as before #1297.
pub(crate) fn field_concurrency(nodes: Option<u64>) -> usize {
    let Some(nodes) = nodes else {
        return FIELD_CONCURRENCY;
    };
    let field = nodes.saturating_mul(size_of::<f32>() as u64).max(1);
    usize::try_from(FIELD_MEMORY / field)
        .unwrap_or(usize::MAX)
        .clamp(FIELD_CONCURRENCY, MAX_FIELD_JOBS)
}

static IO_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("grib-io")
        .enable_all()
        .build()
        .expect("GRIB I/O runtime")
});

/// Run `work` in the background on the GRIB I/O runtime, inside
/// `block_in_place` on one of its workers, so its storage calls take that
/// runtime's handle (root Critical Rule 7). Never on the caller's thread:
/// the map read-ahead (#1335) starts from a render, which does not wait
/// for it.
pub(crate) fn spawn_background(work: impl FnOnce() + Send + 'static) {
    drop(IO_RUNTIME.spawn(async move { tokio::task::block_in_place(work) }));
}

pub(crate) fn run_fetches<F>(future: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    ds_executor::field_jobs::block_on(IO_RUNTIME.handle(), future)
}

/// Run owned field jobs on the dedicated query runtime, `concurrency` at a
/// time (the query's [`crate::FieldPlan`]). Workers return small
/// samples/subsets rather than retaining all decoded global grids. On error
/// or deadline, stop dispatching and drain before the query releases
/// admission. The scheduler is ds-executor's, shared with engine-netcdf
/// (#1067); a caller outside any runtime falls back to the GRIB I/O runtime.
pub(crate) fn run_field_jobs<J, T>(
    concurrency: usize,
    fields: impl Iterator<Item = J> + Send,
    work: impl Fn(J) -> Result<T, DataServerError> + Send + Sync + 'static,
    consume: impl FnMut(T) -> Result<(), DataServerError> + Send,
) -> Result<(), DataServerError>
where
    J: Send + 'static,
    T: Send + 'static,
{
    ds_executor::field_jobs::run_field_jobs(IO_RUNTIME.handle(), concurrency, fields, work, consume)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The memory bound over the grids GRIB sources publish: small, 0.25°
    /// and 0.125° grids run the maximum, 0.1° ten, a 0.05° grid and an
    /// unknown one the default.
    #[test]
    fn field_concurrency_divides_the_memory_bound_by_the_field() {
        assert_eq!(field_concurrency(None), FIELD_CONCURRENCY);
        assert_eq!(field_concurrency(Some(0)), MAX_FIELD_JOBS);
        assert_eq!(field_concurrency(Some(4)), MAX_FIELD_JOBS);
        assert_eq!(field_concurrency(Some(1440 * 721)), MAX_FIELD_JOBS);
        assert_eq!(field_concurrency(Some(2880 * 1441)), MAX_FIELD_JOBS);
        assert_eq!(field_concurrency(Some(3600 * 1801)), 10);
        assert_eq!(field_concurrency(Some(7200 * 3601)), FIELD_CONCURRENCY);
        assert_eq!(field_concurrency(Some(u64::MAX)), FIELD_CONCURRENCY);
    }
}
