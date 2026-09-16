//! Byte admission for complete location responses. Reservations follow the
//! actual allocation through Bytes clones, middleware and response delivery.
use std::io::{self, Write};
use std::sync::{Arc, LazyLock};

use axum::body::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const UNIT: usize = 64 * 1024;
const MIB: usize = 1024 * 1024;
static MEMORY: LazyLock<Arc<Semaphore>> = LazyLock::new(|| {
    let mb = env_usize("MC_EDR_LOCATIONS_MEMORY_MB", 128);
    Arc::new(Semaphore::new(
        mb.saturating_mul(MIB / UNIT).min(u32::MAX as usize),
    ))
});
static LIMIT: LazyLock<usize> = LazyLock::new(|| env_usize("MC_EDR_LOCATIONS_MAX_BYTES", 16 * MIB));

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Memory,
    Limit,
    Cancelled,
}

struct Allocation {
    data: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for Allocation {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

pub(crate) struct Writer<'a> {
    memory: Arc<Semaphore>,
    allocation: Allocation,
    limit: usize,
    cancelled: &'a dyn Fn() -> bool,
    pub(crate) failure: Option<Failure>,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(cancelled: &'a dyn Fn() -> bool) -> Self {
        Self::with_budget(MEMORY.clone(), *LIMIT, cancelled)
    }

    fn with_budget(memory: Arc<Semaphore>, limit: usize, cancelled: &'a dyn Fn() -> bool) -> Self {
        let permit = memory
            .clone()
            .try_acquire_many_owned(0)
            .expect("open memory semaphore");
        Self {
            memory,
            allocation: Allocation {
                data: Vec::new(),
                _permit: permit,
            },
            limit,
            cancelled,
            failure: None,
        }
    }

    pub(crate) fn into_bytes(self) -> Bytes {
        Bytes::from_owner(self.allocation)
    }

    fn fail(&mut self, failure: Failure) -> io::Error {
        self.failure = Some(failure);
        io::Error::other("location response budget exhausted")
    }
}

impl Write for Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(self.fail(Failure::Cancelled));
        }
        let Some(needed) = self
            .allocation
            .data
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
        else {
            return Err(self.fail(Failure::Limit));
        };
        if needed > self.allocation.data.capacity() {
            let capacity = needed
                .max(self.allocation.data.capacity().saturating_mul(2))
                .max(UNIT)
                .min(self.limit);
            let units = capacity.div_ceil(UNIT);
            let permit = u32::try_from(units)
                .ok()
                .and_then(|n| self.memory.clone().try_acquire_many_owned(n).ok())
                .ok_or_else(|| self.fail(Failure::Memory))?;
            // Reserve the entire NEW allocation while the old one is still
            // charged. Vec reallocation's temporary old+new peak is bounded,
            // not merely the final response length. Never await memory here:
            // several growing responses must not deadlock holding old buffers.
            let mut data = Vec::new();
            data.try_reserve_exact(capacity)
                .map_err(|_| self.fail(Failure::Memory))?;
            data.extend_from_slice(&self.allocation.data);
            self.allocation = Allocation {
                data,
                _permit: permit,
            };
        }
        self.allocation.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request},
        middleware,
        routing::get,
        Router,
    };
    use tower::ServiceExt;

    #[test]
    fn growth_charges_old_and_new_allocations_and_releases_on_failure() {
        let pool = Arc::new(Semaphore::new(2));
        let mut writer = Writer::with_budget(pool.clone(), 4 * UNIT, &|| false);
        writer.write_all(&vec![0; UNIT]).unwrap();
        assert_eq!(pool.available_permits(), 1);
        // Growing 1→2 units needs three units during the copy, not two.
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.failure, Some(Failure::Memory));
        assert_eq!(pool.available_permits(), 1);
        drop(writer);
        assert_eq!(pool.available_permits(), 2);
    }

    #[test]
    fn size_limit_and_cancellation_stop_before_allocating() {
        let pool = Arc::new(Semaphore::new(4));
        let mut writer = Writer::with_budget(pool.clone(), 3, &|| false);
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"d").is_err());
        assert_eq!(writer.failure, Some(Failure::Limit));
        assert_eq!(writer.allocation.data, b"abc");
        drop(writer);
        let mut cancelled = Writer::with_budget(pool.clone(), UNIT, &|| true);
        assert!(cancelled.write_all(b"x").is_err());
        assert_eq!(cancelled.failure, Some(Failure::Cancelled));
        assert_eq!(pool.available_permits(), 4);
    }

    #[tokio::test]
    async fn caching_and_body_clones_keep_bytes_charged_and_304_releases_them() {
        let pool = Arc::new(Semaphore::new(1));
        let make_app = |pool: Arc<Semaphore>| {
            Router::new()
                .route(
                    "/",
                    get(move || {
                        let pool = pool.clone();
                        async move {
                            let mut writer = Writer::with_budget(pool, UNIT, &|| false);
                            writer.write_all(b"{}").unwrap();
                            ([(header::ETAG, "\"test\"")], writer.into_bytes())
                        }
                    }),
                )
                .layer(middleware::from_fn(crate::caching::conditional_get))
        };
        let response = make_app(pool.clone())
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(pool.available_permits(), 0);
        let bytes = axum::body::to_bytes(response.into_body(), UNIT)
            .await
            .unwrap();
        let clone = bytes.clone();
        drop(bytes);
        assert_eq!(
            pool.available_permits(),
            0,
            "client-held bytes still own their permit"
        );
        drop(clone);
        assert_eq!(pool.available_permits(), 1);
        let response = make_app(pool.clone())
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::IF_NONE_MATCH, "\"test\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_MODIFIED);
        assert_eq!(pool.available_permits(), 1);
    }
}
