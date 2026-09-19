//! Shared fallback for synchronous callers outside the server's dedicated runtimes.

use std::sync::LazyLock;

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
