//! Backing off an S3 bucket that answers `503 SlowDown` (#1530).
//!
//! A bucket throttles as a whole. On 2026-11-22 a public bucket refused
//! every GET, on every prefix and old runs included, for 40 minutes after
//! a model run was published, from this server and from a client on
//! another network alike, while HEAD and LIST were answered. object_store
//! retried each refused GET on its own, at full concurrency, and the
//! retries kept the refusals coming.
//!
//! So every S3 store this crate builds is one of its bucket's: the stores
//! of a bucket, from every collection and engine, share one
//! [`HostThrottle`] ([`Kind::Bucket`]), one [`AdaptiveLimit`] and one pair
//! of request gauges, as the stores of an HTTP host
//! [`crate::build_http_store_limited`] built share theirs. A bucket that
//! throttles, three `SlowDown`s in 10 s (`throttle::BUCKET_TRIGGER`), is
//! backed off as a 429 backs off an HTTP host (`throttle.rs`): a
//! `Retry-After` pauses its new requests, its requests start the pace
//! apart, a refused request's retry waits the retry delay, and the limit
//! halves. A stray `SlowDown` is only counted.
//!
//! The bucket is known by its canonical label (`traced::s3_label`,
//! `host[:port]/bucket`, an AWS endpoint by its region), which the
//! `storage` span, [`crate::host_requests`] and `/metrics` name it by: one
//! series per bucket, never per prefix, however its stores spell its
//! endpoint. It lives as long as a store on it: a bucket a reload leaves
//! without one goes from [`crate::host_requests`], and a store built on it
//! later starts afresh.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use object_store::client::{HttpClient, HttpConnector, ReqwestConnector};
use object_store::{ClientOptions, ObjectStore};

use crate::gauge::{Gauged, HostGauges, HostRequests};
use crate::retryable::{Client, Counted};
use crate::throttle::{AdaptiveLimit, HostThrottle, Kind, Limited, Throttled};

/// The most requests an S3 bucket allows in flight at once, from every
/// store on it; it halves while the bucket throttles and grows back
/// ([`AdaptiveLimit`]).
///
/// Above what this server's readers ask of one bucket today: GeoTIFF's
/// tile fetches 16 (`MC_COG_TILE_CONCURRENCY`, one pool for the process),
/// the satellite scan's tile reads 16, a GRIB collection's index reads 8, a
/// Zarr preload 8. Requests waiting for a limit at this ceiling are logged
/// ([`AdaptiveLimit::warning_when_full`]).
pub const S3_REQUEST_LIMIT: usize = 64;

/// The limit a bucket starts at, growing to [`S3_REQUEST_LIMIT`] by 8 each
/// 30 s, in three minutes. What one reader asks at most, so a reader's
/// burst passes untouched, while several collections starting on one
/// bucket at once queue: three GRIB collections' 24 index reads go out 16
/// at a time instead of 24, before the bucket has said anything (#1530).
pub(crate) const S3_INITIAL_LIMIT: usize = 16;

/// What the S3 stores of one bucket share.
#[derive(Clone)]
pub(crate) struct Bucket {
    pub throttle: Arc<HostThrottle>,
    pub adaptive: Arc<AdaptiveLimit>,
    pub gauges: Arc<HostGauges>,
}

/// One bucket in [`BUCKETS`]: its throttle held weakly, by its stores, so
/// the entry goes with the last of them.
struct Entry {
    throttle: Weak<HostThrottle>,
    adaptive: Arc<AdaptiveLimit>,
    gauges: Arc<HostGauges>,
}

impl Entry {
    fn bucket(&self) -> Option<Bucket> {
        Some(Bucket {
            throttle: self.throttle.upgrade()?,
            adaptive: self.adaptive.clone(),
            gauges: self.gauges.clone(),
        })
    }
}

/// Every bucket a store is built on, by its label.
static BUCKETS: LazyLock<Mutex<BTreeMap<String, Entry>>> = LazyLock::new(Default::default);

/// The bucket `name` that `label` names, at an endpoint whose URL path is
/// `endpoint_path` (empty, or `/s3` for `https://minio.example/s3`): the
/// one its stores share, or a new one when it has none. The endpoint path
/// of the first store is the bucket's.
pub(crate) fn bucket(label: &str, name: &str, endpoint_path: &str) -> Bucket {
    let mut buckets = BUCKETS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(bucket) = buckets.get(label).and_then(Entry::bucket) {
        return bucket;
    }
    let kind = Kind::bucket(name, endpoint_path);
    let adaptive = Arc::new(
        AdaptiveLimit::new(label.to_string(), S3_REQUEST_LIMIT, &kind)
            .starting_at(S3_INITIAL_LIMIT)
            .warning_when_full(),
    );
    let throttle = Arc::new(HostThrottle::new(label.to_string(), kind));
    throttle.add_limit(adaptive.clone());
    let gauges = Arc::new(HostGauges::default());
    buckets.retain(|_, entry| entry.throttle.strong_count() > 0);
    buckets.insert(
        label.to_string(),
        Entry {
            throttle: Arc::downgrade(&throttle),
            adaptive: adaptive.clone(),
            gauges: gauges.clone(),
        },
    );
    Bucket {
        throttle,
        adaptive,
        gauges,
    }
}

/// The URL path of an S3 endpoint, without a trailing `/`: empty for most,
/// `/s3` for `https://minio.example/s3`. A request names the bucket after
/// it.
pub(crate) fn endpoint_path(endpoint: Option<&str>) -> String {
    endpoint
        .and_then(|endpoint| url::Url::parse(endpoint).ok())
        .map(|url| url.path().trim_end_matches('/').to_string())
        .unwrap_or_default()
}

/// A [`crate::DataStore`] on the S3 store `build` makes for the bucket
/// `name`, labelled `label`, at an endpoint whose URL path is
/// `endpoint_path`: `build` is handed the bucket's throttle for its
/// connector ([`Connector`]), and its store is put under the bucket's
/// limit and gauges.
pub(crate) fn s3_store(
    label: &str,
    name: &str,
    endpoint_path: &str,
    build: impl FnOnce(
        &Arc<HostThrottle>,
    ) -> Result<Arc<dyn ObjectStore>, ds_core::error::DataServerError>,
) -> Result<crate::DataStore, ds_core::error::DataServerError> {
    let bucket = bucket(label, name, endpoint_path);
    let counted: Arc<dyn ObjectStore> = Arc::new(Gauged {
        inner: build(&bucket.throttle)?,
        count: bucket.gauges.in_flight.clone(),
    });
    let limited: Arc<dyn ObjectStore> = Arc::new(Limited {
        inner: counted,
        limit: bucket.adaptive.clone(),
    });
    let store: Arc<dyn ObjectStore> = Arc::new(Gauged {
        inner: limited,
        count: bucket.gauges.requested.clone(),
    });
    let labels = crate::traced::Labels {
        store: label.into(),
        collection: None,
        background: false,
    };
    Ok(crate::DataStore::layered(
        store,
        Some(labels),
        Some(bucket.throttle),
        Default::default(),
    ))
}

/// An S3 store's connector: object_store's own client, built from the
/// store's options as object_store builds it, with every request counted
/// (`retryable.rs`) and, when it names the bucket, sent under the bucket's
/// throttle ([`Throttled`]). A credential provider's requests, which
/// object_store sends through the same connector, name no bucket and pass.
#[derive(Debug)]
pub(crate) struct Connector(pub Arc<HostThrottle>);

impl HttpConnector for Connector {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        let client = ReqwestConnector::default().connect(options)?;
        Ok(HttpClient::new(Throttled {
            inner: Counted(Client(client)),
            host: self.0.clone(),
        }))
    }
}

/// Add the buckets whose label is `wanted` and that a store is still built
/// on to `by_host`, as [`crate::host_requests`] reports them; forget the
/// others.
pub(crate) fn requests_to(
    wanted: &impl Fn(&str) -> bool,
    by_host: &mut BTreeMap<String, HostRequests>,
) {
    let mut buckets = BUCKETS.lock().unwrap_or_else(PoisonError::into_inner);
    buckets.retain(|_, entry| entry.throttle.strong_count() > 0);
    for (label, entry) in buckets.iter().filter(|(label, _)| wanted(label)) {
        let Some(throttle) = entry.throttle.upgrade() else {
            continue;
        };
        let (in_flight, queued) = entry.gauges.read();
        by_host.insert(
            label.clone(),
            HostRequests {
                host: label.clone(),
                limit: entry.adaptive.limits(tokio::time::Instant::now()).0,
                in_flight,
                queued,
                throttled: throttle.answers(),
            },
        );
    }
}
