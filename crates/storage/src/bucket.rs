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
//! [`HostThrottle`] ([`Kind::Bucket`]), one [`AdaptiveLimit`] of
//! [`S3_REQUEST_LIMIT`] and one pair of request gauges, as the stores of an
//! HTTP host [`crate::build_http_store_limited`] built share theirs. A
//! `503` or a `429` from the bucket then backs it off as a 429 backs off an
//! HTTP host (`throttle.rs`): a `Retry-After` pauses its new requests, its
//! requests start the pace apart, the refused request is retried after the
//! retry delay, and the limit halves. The bucket is labelled as the
//! `storage` span labels it, `host[:port]/bucket`, in
//! [`crate::host_requests`] and so in `/metrics`: one series per configured
//! bucket, never per prefix.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use object_store::client::{HttpClient, HttpConnector, ReqwestConnector};
use object_store::{ClientOptions, ObjectStore};

use crate::gauge::{Gauged, HostGauges, HostRequests};
use crate::retryable::{Client, Counted};
use crate::throttle::{AdaptiveLimit, HostThrottle, Kind, Limited, Throttled};

/// The requests an S3 bucket allows in flight at once, from every store on
/// it, until it throttles; then the limit halves and grows back
/// ([`AdaptiveLimit`]).
///
/// Above what this server's readers ask of one bucket today, so it binds
/// only after a cut: GeoTIFF's tile fetches 16 (`MC_COG_TILE_CONCURRENCY`,
/// one pool for the process), the satellite scan's tile reads 16, a GRIB
/// collection's index reads 8, a Zarr preload 8. From 64, the halvings of
/// four seconds of `SlowDown`s take it to 4.
pub(crate) const S3_REQUEST_LIMIT: usize = 64;

/// What the S3 stores of one bucket share.
#[derive(Clone)]
pub(crate) struct Bucket {
    pub throttle: Arc<HostThrottle>,
    pub adaptive: Arc<AdaptiveLimit>,
    pub gauges: Arc<HostGauges>,
}

/// Every bucket an S3 store was built for, by its label: one entry per
/// configured bucket, kept for the life of the process, as the HTTP host
/// stores are.
static BUCKETS: LazyLock<Mutex<BTreeMap<String, Bucket>>> = LazyLock::new(Default::default);

/// The bucket `name` that `label` (`host[:port]/bucket`) names, made the
/// first time a store is built for it.
pub(crate) fn bucket(label: &str, name: &str) -> Bucket {
    let mut buckets = BUCKETS.lock().unwrap_or_else(PoisonError::into_inner);
    buckets
        .entry(label.to_string())
        .or_insert_with(|| {
            let kind = Kind::Bucket(name.to_string());
            let adaptive = Arc::new(AdaptiveLimit::new(
                label.to_string(),
                S3_REQUEST_LIMIT,
                &kind,
            ));
            let throttle = Arc::new(HostThrottle::new(label.to_string(), kind));
            throttle.add_limit(adaptive.clone());
            Bucket {
                throttle,
                adaptive,
                gauges: Arc::new(HostGauges::default()),
            }
        })
        .clone()
}

/// A [`crate::DataStore`] on the S3 store `build` makes for the bucket
/// `name`, labelled `label`: `build` is handed the bucket's throttle for
/// its connector ([`Connector`]), and its store is put under the bucket's
/// limit and gauges.
pub(crate) fn s3_store(
    label: &str,
    name: &str,
    build: impl FnOnce(
        &Arc<HostThrottle>,
    ) -> Result<Arc<dyn ObjectStore>, ds_core::error::DataServerError>,
) -> Result<crate::DataStore, ds_core::error::DataServerError> {
    let bucket = bucket(label, name);
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

/// Add the buckets whose label is `wanted` to `by_host`, as
/// [`crate::host_requests`] reports them.
pub(crate) fn requests_to(
    wanted: &impl Fn(&str) -> bool,
    by_host: &mut BTreeMap<String, HostRequests>,
) {
    let buckets = BUCKETS.lock().unwrap_or_else(PoisonError::into_inner);
    for (label, bucket) in buckets.iter().filter(|(label, _)| wanted(label)) {
        let (in_flight, queued) = bucket.gauges.read();
        by_host.insert(
            label.clone(),
            HostRequests {
                host: label.clone(),
                limit: bucket.adaptive.limits(tokio::time::Instant::now()).0,
                in_flight,
                queued,
                throttled: bucket.throttle.answers(),
            },
        );
    }
}
