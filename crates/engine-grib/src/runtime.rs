//! Shared fallback for synchronous callers outside the server's dedicated runtimes.

use std::sync::{Arc, LazyLock};

use ds_core::error::DataServerError;
use tokio::task::JoinSet;

/// Per admitted query: bound field I/O, decoding, and live uncached grids.
pub(crate) const FIELD_CONCURRENCY: usize = 4;

static IO_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("grib-io")
        .enable_all()
        .build()
        .expect("GRIB I/O runtime")
});

pub(crate) fn run_fetches<F: std::future::Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => IO_RUNTIME.block_on(future),
    }
}

/// Run owned field jobs on the dedicated query runtime. Workers return small
/// samples/subsets rather than retaining all decoded global grids. On error or
/// deadline, stop dispatching and drain before the query releases admission.
pub(crate) fn run_field_jobs<J, T>(
    fields: impl Iterator<Item = J>,
    work: impl Fn(J) -> Result<T, DataServerError> + Send + Sync + 'static,
    mut consume: impl FnMut(T) -> Result<(), DataServerError>,
) -> Result<(), DataServerError>
where
    J: Send + 'static,
    T: Send + 'static,
{
    let mut fields = fields.fuse();
    let work = Arc::new(work);
    let deadline = ds_core::deadline::current();
    run_fetches(async {
        let mut jobs = JoinSet::new();
        let mut fatal = None;
        loop {
            if let Err(error) = ds_core::deadline::check() {
                fatal = Some(error);
            }
            while jobs.len() < FIELD_CONCURRENCY && fatal.is_none() {
                let Some(field) = fields.next() else { break };
                let work = work.clone();
                jobs.spawn(async move {
                    // Single-flight cache waits are synchronous as well as
                    // storage reads; free the runtime worker for the whole job.
                    tokio::task::block_in_place(|| {
                        let _deadline = ds_core::deadline::enter(deadline);
                        ds_core::deadline::check()?;
                        let result = work(field);
                        ds_core::deadline::check()?;
                        result
                    })
                });
            }
            let Some(result) = jobs.join_next().await else {
                break;
            };
            let result = result.unwrap_or_else(|e| {
                Err(DataServerError::Engine(format!(
                    "GRIB field worker failed: {e}"
                )))
            });
            if fatal.is_none() {
                if let Err(error) = result.and_then(&mut consume) {
                    fatal = Some(error);
                }
            }
        }
        match fatal {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })
}
