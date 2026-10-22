//! The EDR response memory budget (#1041): byte admission for the bodies EDR
//! encodes from a query (CoverageJSON, EDR GeoJSON, CSV and HTML) and for
//! `/locations`. One process-wide [`ByteBudget`],
//! `MC_EDR_RESPONSE_MEMORY_MB` (default 512 MiB). It never waits: a body
//! that does not fit fails at once, and the handler answers 503 `ServerBusy`.
//! Reservations follow the actual allocation through `Bytes` clones,
//! middleware and response delivery: a body stays charged until its last
//! clone drops, after the client has read it or gone away.
use std::io::{self, Write};
use std::sync::{Arc, LazyLock};

use axum::body::Bytes;
use ds_executor::budget::{ByteBudget, BytePermit};

/// The smallest buffer a [`Writer`] allocates, and how many bytes it writes
/// between cancellation checks.
const UNIT: usize = 64 * 1024;
const MIB: usize = 1024 * 1024;

/// `/locations` had a budget of its own before #1041.
const RETIRED_LOCATIONS_MEMORY: &str = "MC_EDR_LOCATIONS_MEMORY_MB";

static BUDGET: LazyLock<Arc<ByteBudget>> = LazyLock::new(|| {
    if std::env::var_os(RETIRED_LOCATIONS_MEMORY).is_some() {
        tracing::warn!(
            "{RETIRED_LOCATIONS_MEMORY} is retired and ignored: /locations shares the EDR \
             response memory budget, MC_EDR_RESPONSE_MEMORY_MB (default 512)"
        );
    }
    let bytes = env_usize("MC_EDR_RESPONSE_MEMORY_MB", 512).saturating_mul(MIB);
    Arc::new(ByteBudget::new(u64::try_from(bytes).unwrap_or(u64::MAX)))
});

/// The per-response cap of a `/locations` body.
static LOCATIONS_LIMIT: LazyLock<usize> =
    LazyLock::new(|| env_usize("MC_EDR_LOCATIONS_MAX_BYTES", 16 * MIB));

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Read the budget's environment now, so a retired variable is reported
/// when the server starts rather than at its first response.
pub(crate) fn init() {
    LazyLock::force(&BUDGET);
}

/// (reserved bytes, capacity bytes, rejected responses), for `/metrics`.
pub fn metrics() -> (u64, u64, u64) {
    BUDGET.metrics()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The budget is in use: 503 `ServerBusy`, retry later.
    Memory,
    /// Over the body's own cap (`/locations` only).
    Limit,
    /// The query's deadline passed, or its client went away.
    Cancelled,
}

struct Allocation {
    data: Vec<u8>,
    _permit: Option<BytePermit>,
}

impl AsRef<[u8]> for Allocation {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

/// A body written through `io::Write`, charged for every buffer it
/// allocates before allocating it.
pub(crate) struct Writer<'a> {
    budget: Arc<ByteBudget>,
    allocation: Allocation,
    limit: usize,
    cancelled: &'a dyn Fn() -> bool,
    /// The length at which `cancelled` is next asked: serde_json writes
    /// every token separately, so not on every write.
    next_check: usize,
    pub(crate) failure: Option<Failure>,
}

impl<'a> Writer<'a> {
    /// A data query's body. It has no size cap of its own: the value caps
    /// bound it.
    pub(crate) fn new(cancelled: &'a dyn Fn() -> bool) -> Self {
        Self::with_budget(BUDGET.clone(), usize::MAX, cancelled)
    }

    /// A `/locations` body, also capped per response by
    /// `MC_EDR_LOCATIONS_MAX_BYTES` (default 16 MiB).
    pub(crate) fn locations(cancelled: &'a dyn Fn() -> bool) -> Self {
        Self::with_budget(BUDGET.clone(), *LOCATIONS_LIMIT, cancelled)
    }

    fn with_budget(budget: Arc<ByteBudget>, limit: usize, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            budget,
            allocation: Allocation {
                data: Vec::new(),
                _permit: None,
            },
            limit,
            cancelled,
            next_check: 0,
            failure: None,
        }
    }

    pub(crate) fn into_bytes(self) -> Bytes {
        Bytes::from_owner(self.allocation)
    }

    fn fail(&mut self, failure: Failure) -> io::Error {
        self.failure = Some(failure);
        io::Error::other("EDR response budget exhausted")
    }
}

impl Write for Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let len = self.allocation.data.len();
        if len >= self.next_check {
            if (self.cancelled)() {
                return Err(self.fail(Failure::Cancelled));
            }
            self.next_check = len.saturating_add(UNIT);
        }
        let Some(needed) = len.checked_add(bytes.len()).filter(|n| *n <= self.limit) else {
            return Err(self.fail(Failure::Limit));
        };
        if needed > self.allocation.data.capacity() {
            let capacity = needed
                .max(self.allocation.data.capacity().saturating_mul(2))
                .max(UNIT)
                .min(self.limit);
            let permit = u64::try_from(capacity)
                .ok()
                .and_then(|n| self.budget.reserve(n).ok())
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
                _permit: Some(permit),
            };
        }
        self.allocation.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Bytes reserved before a body is built in a buffer of its own: an HTML
/// page, whose size is measured from above before it renders.
pub(crate) struct Reservation {
    budget: Arc<ByteBudget>,
    permit: BytePermit,
    bytes: usize,
}

/// Reserve `bytes` for a body about to be built, or fail before anything
/// is allocated.
pub(crate) fn reserve(bytes: usize) -> Result<Reservation, Failure> {
    reserve_from(&BUDGET, bytes)
}

fn reserve_from(budget: &Arc<ByteBudget>, bytes: usize) -> Result<Reservation, Failure> {
    let permit = u64::try_from(bytes)
        .ok()
        .and_then(|n| budget.reserve(n).ok())
        .ok_or(Failure::Memory)?;
    Ok(Reservation {
        budget: budget.clone(),
        permit,
        bytes,
    })
}

impl Reservation {
    /// The body built in `data`, charged until its last clone drops. A
    /// buffer that outgrew the reservation is charged its whole capacity
    /// instead, or fails.
    pub(crate) fn into_bytes(self, data: Vec<u8>) -> Result<Bytes, Failure> {
        let permit = if data.capacity() <= self.bytes {
            self.permit
        } else {
            reserve_from(&self.budget, data.capacity())?.permit
        };
        Ok(Bytes::from_owner(Allocation {
            data,
            _permit: Some(permit),
        }))
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
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn used(budget: &ByteBudget) -> u64 {
        budget.metrics().0
    }

    #[test]
    fn growth_charges_old_and_new_allocations_and_releases_on_failure() {
        let budget = Arc::new(ByteBudget::new(2 * UNIT as u64));
        let mut writer = Writer::with_budget(budget.clone(), 4 * UNIT, &|| false);
        writer.write_all(&vec![0; UNIT]).unwrap();
        assert_eq!(used(&budget), UNIT as u64);
        // Growing 1→2 units needs three units during the copy, not two.
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.failure, Some(Failure::Memory));
        assert_eq!(used(&budget), UNIT as u64);
        assert_eq!(budget.metrics().2, 1, "one rejected response");
        drop(writer);
        assert_eq!(used(&budget), 0);
    }

    #[test]
    fn size_limit_and_cancellation_stop_before_allocating() {
        let budget = Arc::new(ByteBudget::new(4 * UNIT as u64));
        let mut writer = Writer::with_budget(budget.clone(), 3, &|| false);
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"d").is_err());
        assert_eq!(writer.failure, Some(Failure::Limit));
        assert_eq!(writer.allocation.data, b"abc");
        drop(writer);
        let mut cancelled = Writer::with_budget(budget.clone(), UNIT, &|| true);
        assert!(cancelled.write_all(b"x").is_err());
        assert_eq!(cancelled.failure, Some(Failure::Cancelled));
        assert_eq!(used(&budget), 0);
    }

    /// Cancellation is asked once per [`UNIT`] written, so a body notices
    /// an expired query within one unit.
    #[test]
    fn cancellation_is_checked_once_per_unit() {
        let budget = Arc::new(ByteBudget::new(4 * UNIT as u64));
        let asked = std::cell::Cell::new(0);
        let expired = std::cell::Cell::new(false);
        let cancelled = || {
            asked.set(asked.get() + 1);
            expired.get()
        };
        let mut writer = Writer::with_budget(budget, usize::MAX, &cancelled);
        for _ in 0..UNIT {
            writer.write_all(b"x").unwrap();
        }
        assert_eq!(asked.get(), 1);
        expired.set(true);
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.failure, Some(Failure::Cancelled));
    }

    #[test]
    fn a_reservation_is_charged_until_its_body_drops() {
        let budget = Arc::new(ByteBudget::new(100));
        assert_eq!(
            reserve_from(&budget, 101).err(),
            Some(Failure::Memory),
            "a page over the budget fails before it allocates"
        );
        let reservation = reserve_from(&budget, 60).unwrap();
        assert_eq!(used(&budget), 60);
        assert_eq!(reserve_from(&budget, 41).err(), Some(Failure::Memory));
        let body = reservation.into_bytes(Vec::with_capacity(60)).unwrap();
        assert_eq!(used(&budget), 60);
        drop(body);
        assert_eq!(used(&budget), 0);
        // A buffer that outgrew its reservation is charged all of it.
        let grown = reserve_from(&budget, 10)
            .unwrap()
            .into_bytes(Vec::with_capacity(80))
            .unwrap();
        assert_eq!(used(&budget), 80);
        drop(grown);
        assert!(reserve_from(&budget, 10)
            .unwrap()
            .into_bytes(Vec::with_capacity(101))
            .is_err());
        assert_eq!(used(&budget), 0);
    }

    /// A client that goes away mid-body: the server drops the response
    /// body, read or not, and its bytes are released with it.
    #[tokio::test]
    async fn a_dropped_body_releases_its_bytes() {
        let budget = Arc::new(ByteBudget::new(4 * UNIT as u64));
        let body = || {
            let mut writer = Writer::with_budget(budget.clone(), usize::MAX, &|| false);
            writer.write_all(&[b'x'; 1000]).unwrap();
            Body::from(writer.into_bytes())
        };
        drop(body());
        assert_eq!(used(&budget), 0);
        let mut stream = body();
        assert_eq!(used(&budget), UNIT as u64);
        // The frame being sent keeps the bytes charged until it drops.
        let frame = stream.frame().await.unwrap().unwrap();
        drop(stream);
        assert_eq!(used(&budget), UNIT as u64);
        drop(frame);
        assert_eq!(used(&budget), 0);
    }

    #[tokio::test]
    async fn caching_and_body_clones_keep_bytes_charged_and_304_releases_them() {
        let budget = Arc::new(ByteBudget::new(UNIT as u64));
        let make_app = |budget: Arc<ByteBudget>| {
            Router::new()
                .route(
                    "/",
                    get(move || {
                        let budget = budget.clone();
                        async move {
                            let mut writer = Writer::with_budget(budget, UNIT, &|| false);
                            writer.write_all(b"{}").unwrap();
                            ([(header::ETAG, "\"test\"")], writer.into_bytes())
                        }
                    }),
                )
                .layer(middleware::from_fn(crate::caching::conditional_get))
        };
        let response = make_app(budget.clone())
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(used(&budget), UNIT as u64);
        let bytes = axum::body::to_bytes(response.into_body(), UNIT)
            .await
            .unwrap();
        let clone = bytes.clone();
        drop(bytes);
        assert_eq!(
            used(&budget),
            UNIT as u64,
            "client-held bytes still own their permit"
        );
        drop(clone);
        assert_eq!(used(&budget), 0);
        let response = make_app(budget.clone())
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
        assert_eq!(used(&budget), 0);
    }
}
