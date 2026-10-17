//! The persistent runtime remote reads run on, the bridge a synchronous
//! reader crosses into it, and the fetch mode a thread reads under.
//!
//! hdf5-reader's storage callback is synchronous; an HTTP range read is a
//! future. [`blocking`] runs a call that drives one on [`handle`] with
//! `Handle::block_on` from any thread a read can come from (root CLAUDE.md
//! Critical Rule 7):
//!
//! - an async worker of a multi-thread runtime (a render on a request
//!   worker, the poll loop on `poll_runtime()`): inside `block_in_place`,
//!   so the worker's other tasks move on;
//! - a blocking-pool thread (a `RenderJob`, a detached fill on this
//!   runtime): `block_in_place` runs the call as it is;
//! - a thread without a runtime (ingest's open workers): directly;
//! - a current-thread runtime, which can neither `block_in_place` nor enter
//!   another runtime: on a scoped thread carrying the deadline and mode.
//!
//! Never from a rayon pool: nothing here uses one.

use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::LazyLock;

use ds_core::deadline;
use ds_storage::FetchBudget;

/// Remote reads outlive the requests that start them (a detached fill,
/// [`crate::field`]), and connections are pooled across reads: one
/// runtime for the process, never one per read.
static IO_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("netcdf-io")
        .enable_all()
        .build()
        .expect("NetCDF I/O runtime")
});

/// The runtime remote reads are driven on.
pub(crate) fn handle() -> &'static tokio::runtime::Handle {
    IO_RUNTIME.handle()
}

/// Whose read a remote fetch is, which sets the share of the host's
/// request limit it takes and how long it may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A request waits on it: the host's whole limit, and the request's
    /// deadline, else [`FetchBudget::Request`].
    Request,
    /// A field fill a request started and then left to finish
    /// ([`crate::field`]): the host's whole limit, [`FetchBudget::Background`]
    /// and no deadline, so it outlives a request's 3 s.
    Detached,
    /// Ingest and prewarm on the poll runtime: the background share of the
    /// host's limit, [`FetchBudget::Background`].
    Background,
}

impl Mode {
    pub(crate) fn budget(self) -> FetchBudget {
        match self {
            Mode::Request => FetchBudget::Request,
            Mode::Detached | Mode::Background => FetchBudget::Background,
        }
    }
}

thread_local! { static MODE: Cell<Mode> = const { Cell::new(Mode::Request) }; }

/// The calling thread's mode, [`Mode::Request`] unless entered.
pub(crate) fn mode() -> Mode {
    MODE.get()
}

/// Read under `mode` on this thread until the guard drops.
pub(crate) fn enter(mode: Mode) -> ModeGuard {
    ModeGuard {
        previous: MODE.replace(mode),
        _thread: PhantomData,
    }
}

/// Restores the previous mode; never moves to another thread.
pub(crate) struct ModeGuard {
    previous: Mode,
    _thread: PhantomData<Rc<()>>,
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        MODE.set(self.previous);
    }
}

/// Run `call`, which blocks on [`handle`], from the calling thread: see the
/// module docs. The deadline and mode `call` reads are the caller's.
pub(crate) fn blocking<T: Send>(call: impl FnOnce() -> T + Send) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(current) if current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(call)
        }
        Ok(_) => {
            let (end, mode) = (deadline::current(), mode());
            std::thread::scope(|scope| {
                scope
                    .spawn(move || {
                        let _deadline = deadline::enter(end);
                        let _mode = enter(mode);
                        call()
                    })
                    .join()
                    .expect("NetCDF read thread")
            })
        }
        Err(_) => call(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_here() -> (Option<std::time::Instant>, Mode, i32) {
        blocking(|| {
            let value = handle().block_on(async { 42 });
            (deadline::current(), mode(), value)
        })
    }

    /// Every thread a read comes from reaches the I/O runtime, with its
    /// deadline and mode.
    #[test]
    fn every_calling_thread_reaches_the_runtime() {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let _deadline = deadline::enter(Some(end));
        let background = enter(Mode::Background);
        assert_eq!(read_here(), (Some(end), Mode::Background, 42));

        let current = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(
            current.block_on(async { read_here() }),
            (Some(end), Mode::Background, 42),
            "a current-thread runtime"
        );

        let multi = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let on_worker = multi
            .block_on(multi.spawn(async {
                let _mode = enter(Mode::Request);
                read_here()
            }))
            .unwrap();
        assert_eq!(on_worker, (None, Mode::Request, 42), "an async worker");
        let on_blocking = multi
            .block_on(multi.spawn_blocking(|| {
                let _mode = enter(Mode::Detached);
                read_here()
            }))
            .unwrap();
        assert_eq!(on_blocking, (None, Mode::Detached, 42), "a blocking worker");
        let on_io = handle()
            .block_on(handle().spawn_blocking(read_here))
            .unwrap();
        assert_eq!(on_io.2, 42, "a blocking thread of the I/O runtime itself");
        drop(background);
        assert_eq!(mode(), Mode::Request);
    }
}
