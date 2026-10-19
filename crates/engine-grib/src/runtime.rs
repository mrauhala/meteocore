//! Shared fallback for synchronous callers outside the server's dedicated runtimes.

use std::sync::LazyLock;

use ds_core::error::DataServerError;

/// Per admitted query: bound field I/O, decoding, and live uncached grids.
#[cfg(test)]
pub(crate) use ds_executor::field_jobs::FIELD_CONCURRENCY;

static IO_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("grib-io")
        .enable_all()
        .build()
        .expect("GRIB I/O runtime")
});

pub(crate) fn run_fetches<F>(future: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    ds_executor::field_jobs::block_on(IO_RUNTIME.handle(), future)
}

/// Run owned field jobs on the dedicated query runtime. Workers return small
/// samples/subsets rather than retaining all decoded global grids. On error or
/// deadline, stop dispatching and drain before the query releases admission.
/// The scheduler is ds-executor's, shared with engine-netcdf (#1067); a caller
/// outside any runtime falls back to the GRIB I/O runtime.
pub(crate) fn run_field_jobs<J, T>(
    fields: impl Iterator<Item = J> + Send,
    work: impl Fn(J) -> Result<T, DataServerError> + Send + Sync + 'static,
    consume: impl FnMut(T) -> Result<(), DataServerError> + Send,
) -> Result<(), DataServerError>
where
    J: Send + 'static,
    T: Send + 'static,
{
    ds_executor::field_jobs::run_field_jobs(IO_RUNTIME.handle(), fields, work, consume)
}
