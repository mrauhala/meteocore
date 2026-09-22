//! Deadline-aware sync bridge for the Icechunk backend.
use std::{future::Future, sync::LazyLock, time::Duration};

use ds_core::{deadline, error::DataServerError};

// Clients and background metadata tasks outlive individual storage calls.
// In particular, never construct/drop a runtime for each read on a CLI thread.
static IO_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("zarr-io")
        .enable_all()
        .build()
        .expect("Zarr I/O runtime")
});

pub(crate) fn run<F, T>(future: F) -> Result<T, DataServerError>
where
    F: Future<Output = Result<T, DataServerError>> + Send,
    T: Send,
{
    let request_deadline = deadline::current();
    deadline::check()?;
    let timed = async {
        let end =
            request_deadline.unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(30));
        match tokio::time::timeout_at(end.into(), future).await {
            Ok(result) => result,
            Err(_) if request_deadline.is_some() => Err(DataServerError::DeadlineExceeded),
            Err(_) => Err(DataServerError::Storage(
                "Zarr storage request timed out".into(),
            )),
        }
    };
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| IO_RUNTIME.block_on(timed))
        }
        // A current-thread runtime cannot block_in_place or enter another
        // runtime. Support synchronous library callers there on a scoped thread.
        Ok(_) => std::thread::scope(|scope| {
            scope
                .spawn(|| IO_RUNTIME.block_on(timed))
                .join()
                .expect("Zarr I/O worker")
        }),
        Err(_) => IO_RUNTIME.block_on(timed),
    };
    deadline::check()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn expired_deadline_does_not_poll_io() {
        let _scope = deadline::enter(Some(std::time::Instant::now() - Duration::from_secs(1)));
        let result: Result<(), _> = run(async { panic!("expired I/O was polled") });
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
    }

    #[test]
    fn deadline_drops_inflight_future_and_background_reads_still_work() {
        struct OnDrop<'a>(&'a AtomicBool);
        impl Drop for OnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = AtomicBool::new(false);
        let scope = deadline::enter(Some(std::time::Instant::now() + Duration::from_millis(40)));
        let result: Result<(), _> = run(async {
            let _guard = OnDrop(&dropped);
            std::future::pending().await
        });
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
        assert!(dropped.load(Ordering::SeqCst));
        drop(scope);
        assert_eq!(run(async { Ok(42) }).unwrap(), 42);
    }

    #[tokio::test]
    async fn current_thread_runtime_is_supported() {
        assert_eq!(run(async { Ok(42) }).unwrap(), 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_workers_and_blocking_workers_are_supported() {
        assert_eq!(run(async { Ok(42) }).unwrap(), 42);
        assert_eq!(
            tokio::task::spawn_blocking(|| run(async { Ok(43) }))
                .await
                .unwrap()
                .unwrap(),
            43
        );
    }
}
