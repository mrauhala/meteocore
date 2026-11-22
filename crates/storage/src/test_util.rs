//! Test support for crates that read through a [`DataStore`] (#1055): a
//! store whose GET bodies stall until the test lets them finish, and
//! [`run_stalled`], which runs a synchronous call through such a store on a
//! paused clock and moves the clock [`STALL`] past each stalled GET. A test
//! pins with them which [`FetchBudget`] a caller passes: the stall outlasts
//! [`FetchBudget::Request`], not [`FetchBudget::Background`], so a read under
//! the wrong budget fails or succeeds where the test expects the other.
//! And [`SlowReads`], a store whose every GET takes a set time and counts
//! as in flight meanwhile, for a test of how many reads a caller overlaps.
//! And [`FailingReads`], a store whose GETs fail as a store that kept
//! answering one status through every retry, for a test of what a caller
//! answers an unavailable store (#1523). And [`ThrottledBucket`], a back-off
//! a test drives, for a caller that pauses while its bucket backs off
//! (#1530).
//!
//! Compiled for ds-storage's own tests and, through its `test-util`
//! feature, for the tests of a crate that enables the feature on its
//! ds-storage dev-dependency. Never in a server build.
//!
//! [`DataStore`]: crate::DataStore

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectMeta, ObjectStore, ObjectStoreExt, PutPayload};

use crate::FetchBudget;

/// How long [`run_stalled`] lets each stalled GET take: longer than
/// [`FetchBudget::Request`] allows, shorter than [`FetchBudget::Background`].
pub const STALL: Duration = Duration::from_secs(60);

const _: () = assert!(
    STALL.as_secs() > FetchBudget::Request.timeout().as_secs()
        && STALL.as_secs() < FetchBudget::Background.timeout().as_secs()
);

/// An in-memory store whose every GET answers at once with the first half
/// of its body, while the rest waits until [`run_stalled`] lets it
/// through, after moving the clock [`STALL`] ahead: a GET whose body
/// stalls, as seen from outside object_store (#1011). HEADs, LISTs and
/// writes answer at once.
#[derive(Debug, Default)]
pub struct StalledBody {
    inner: InMemory,
    gate: Arc<Gate>,
}

/// The stalled GETs of one [`StalledBody`], and whether the call
/// [`run_stalled`] runs has returned. One lock for all of it, so the
/// harness never misses a change it waits for.
#[derive(Debug, Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct GateState {
    /// Every GET that stalled, in the order it stalled.
    stalls: Vec<Stall>,
    done: bool,
}

/// One stalled GET's tail.
#[derive(Debug, Default)]
struct Stall {
    /// How often the harness woke the tail to look at the clock, and the
    /// last of those wake-ups a poll of the tail has seen.
    pokes: u64,
    seen: u64,
    /// The harness let the rest of the body through.
    through: bool,
    /// The GET was dropped: its budget ended.
    gone: bool,
    waker: Option<Waker>,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `change` under the lock, wake every waiter, then the wakers it
    /// returned.
    fn update(&self, change: impl FnOnce(&mut GateState) -> Vec<Waker>) {
        let wakers = change(&mut self.lock());
        self.changed.notify_all();
        wakers.into_iter().for_each(Waker::wake);
    }
}

/// The tail of a stalled GET: pending until the harness lets it through.
/// Each poll records the harness's wake-ups as seen, so the harness knows
/// the GET's caller looked at its budget after the clock moved; dropping it
/// records the GET as gone.
struct Tail {
    gate: Arc<Gate>,
    index: usize,
}

impl Future for Tail {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut through = false;
        self.gate.update(|state| {
            let stall = &mut state.stalls[self.index];
            stall.seen = stall.pokes;
            through = stall.through;
            if !through {
                stall.waker = Some(cx.waker().clone());
            }
            Vec::new()
        });
        if through {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Tail {
    fn drop(&mut self) {
        let index = self.index;
        self.gate.update(|state| {
            let stall = &mut state.stalls[index];
            stall.gone = !stall.through;
            Vec::new()
        });
    }
}

/// Marks the call [`run_stalled`] runs as returned, however it returned.
struct Done(Arc<Gate>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.update(|state| {
            state.done = true;
            Vec::new()
        });
    }
}

impl StalledBody {
    /// A store holding `objects`, as `(key, bytes)`.
    pub fn with_objects<K: AsRef<str>, B: Into<Bytes>>(
        objects: impl IntoIterator<Item = (K, B)>,
    ) -> Arc<Self> {
        let store = Self::default();
        for (key, bytes) in objects {
            store.put(key.as_ref(), bytes);
        }
        Arc::new(store)
    }

    /// Store `bytes` under `key`.
    pub fn put(&self, key: &str, bytes: impl Into<Bytes>) {
        futures::executor::block_on(
            self.inner
                .put(&Path::from(key), PutPayload::from(bytes.into())),
        )
        .expect("an in-memory put");
    }

    /// How many GETs have stalled so far.
    pub fn stalled(&self) -> usize {
        self.gate.lock().stalls.len()
    }
}

/// Run `call` as a caller on a runtime would: on a thread of its own that
/// entered a current-thread runtime whose clock is paused, so the storage
/// bridge drives `call`'s reads on that runtime. Each time GETs of `store`
/// stall, the clock moves [`STALL`] ahead, which fires every fetch budget
/// that ends meanwhile, and then the GETs still waiting get the rest of
/// their bodies. Returns what `call` returned; a panic in `call` is resumed
/// here.
///
/// The clock moves only here, never on its own: a paused runtime's
/// auto-advance jumps to its next timer whenever its own thread idles, even
/// while `call`'s thread is still busy, and would end a budget early. And a
/// stalled GET gets the rest of its body only after its caller has looked
/// at the moved clock: a timeout polls the future it limits first, so a
/// body let through at once would beat the budget that ended before it.
pub fn run_stalled<T: Send + 'static>(
    store: &StalledBody,
    call: impl FnOnce() -> T + Send + 'static,
) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a current-thread runtime");
    let handle = runtime.handle().clone();
    let gate = Arc::clone(&store.gate);
    let mut moved = 0;
    gate.update(|state| {
        state.done = false;
        moved = state.stalls.len();
        Vec::new()
    });
    let done = Done(Arc::clone(&gate));
    let caller = std::thread::spawn(move || {
        let _done = done;
        let _entered = handle.enter();
        call()
    });
    loop {
        let stalled = {
            let state = gate
                .changed
                .wait_while(gate.lock(), |state| {
                    !state.done && state.stalls.len() == moved
                })
                .unwrap_or_else(|e| e.into_inner());
            if state.done {
                break;
            }
            moved..state.stalls.len()
        };
        moved = stalled.end;
        runtime.block_on(tokio::time::advance(STALL));
        // Wake each stalled GET, and wait until its caller has seen the
        // clock: polled the tail again, so that a budget the move ended is
        // seen before the body is let through, or dropped the GET.
        gate.update(|state| {
            state.stalls[stalled.clone()]
                .iter_mut()
                .filter(|stall| !stall.gone)
                .filter_map(|stall| {
                    stall.pokes += 1;
                    stall.waker.take()
                })
                .collect()
        });
        let state = gate
            .changed
            .wait_while(gate.lock(), |state| {
                !state.done
                    && state.stalls[stalled.clone()]
                        .iter()
                        .any(|stall| !stall.gone && stall.seen < stall.pokes)
            })
            .unwrap_or_else(|e| e.into_inner());
        drop(state);
        gate.update(|state| {
            state.stalls[stalled.clone()]
                .iter_mut()
                .filter(|stall| !stall.gone)
                .filter_map(|stall| {
                    stall.through = true;
                    stall.waker.take()
                })
                .collect()
        });
    }
    caller
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

impl std::fmt::Display for StalledBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StalledBody")
    }
}

#[async_trait::async_trait]
impl ObjectStore for StalledBody {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if options.head {
            return self.inner.get_opts(path, options).await;
        }
        let result = self.inner.get_opts(path, options).await?;
        let (meta, range, attributes, extensions) = (
            result.meta.clone(),
            result.range.clone(),
            result.attributes.clone(),
            result.extensions.clone(),
        );
        let mut body = result.bytes().await?;
        let head = body.split_to(body.len() / 2);
        let gate = Arc::clone(&self.gate);
        let tail = futures::stream::once(async move {
            let mut index = 0;
            gate.update(|state| {
                index = state.stalls.len();
                state.stalls.push(Stall::default());
                Vec::new()
            });
            Tail { gate, index }.await;
            Ok(body)
        });
        Ok(object_store::GetResult {
            payload: object_store::GetResultPayload::Stream(
                futures::stream::once(async move { Ok(head) })
                    .chain(tail)
                    .boxed(),
            ),
            meta,
            range,
            attributes,
            extensions,
        })
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// Wraps [`InMemory`]: every GET, a HEAD included, sleeps for the store's
/// delay while it counts as in flight (and, unless a HEAD, as fetching),
/// and records its key when it starts:
/// for the batched range read tests (#1042), and for the engines' tests of
/// how many frames an EDR query reads at once (#1495). The delay can change
/// between reads ([`Self::set_delay`]), so a test builds its engine at once
/// and then times the reads it means.
#[derive(Debug, Default)]
pub struct SlowReads {
    pub inner: InMemory,
    delay: Mutex<Duration>,
    /// GETs in flight now, and the most there ever were.
    pub active: Arc<AtomicUsize>,
    pub peak: Arc<AtomicUsize>,
    /// The same of the GETs that are not HEADs, which fetch bytes.
    pub fetching: Arc<AtomicUsize>,
    pub fetch_peak: Arc<AtomicUsize>,
    /// Every key GET, in the order the GETs started.
    pub started: Mutex<Vec<String>>,
}

impl SlowReads {
    /// An empty store whose every GET takes `delay`.
    pub fn new(delay: Duration) -> Self {
        Self {
            delay: Mutex::new(delay),
            ..Self::default()
        }
    }

    /// Every GET from now on takes `delay`.
    pub fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap_or_else(|e| e.into_inner()) = delay;
    }

    /// Store `bytes` under `key`.
    pub fn put(&self, key: &str, bytes: impl Into<Bytes>) {
        futures::executor::block_on(
            self.inner
                .put(&Path::from(key), PutPayload::from(bytes.into())),
        )
        .expect("an in-memory put");
    }

    /// Forget the GETs so far: the keys started and the peaks in flight.
    pub fn reset(&self) {
        self.started
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.peak.store(0, Ordering::SeqCst);
        self.fetch_peak.store(0, Ordering::SeqCst);
    }

    /// How many GETs have started.
    pub fn reads(&self) -> usize {
        self.started.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The most GETs in flight at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// The most GETs in flight at once that fetch bytes, HEADs left out.
    pub fn fetch_peak(&self) -> usize {
        self.fetch_peak.load(Ordering::SeqCst)
    }
}

impl std::fmt::Display for SlowReads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SlowReads")
    }
}

/// Counts one GET in flight until it finishes or is dropped.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ObjectStore for SlowReads {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.started
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(path.to_string());
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let _in_flight = InFlight(self.active.clone());
        let _fetching = (!options.head).then(|| {
            let now = self.fetching.fetch_add(1, Ordering::SeqCst) + 1;
            self.fetch_peak.fetch_max(now, Ordering::SeqCst);
            InFlight(self.fetching.clone())
        });
        let delay = *self.delay.lock().unwrap_or_else(|e| e.into_inner());
        tokio::time::sleep(delay).await;
        self.inner.get_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// Wraps [`InMemory`]: while a status is set ([`Self::fail_with`]), every
/// GET but a HEAD fails as object_store fails a GET whose every attempt the
/// server answered with that status, retries included (#1523). The error
/// reads as object_store's does, a `Generic` error whose source chain ends
/// in `Server returned non-2xx status code: <status>`, so a
/// [`DataStore`](crate::DataStore) types it as it types the real answer:
/// 429, 502, 503 and 504 are `DataServerError::Unavailable`, any other
/// status a `Storage` error. HEADs, LISTs and writes answer from the store,
/// so a test builds its engine on it first and then lets the reads fail.
#[derive(Debug, Default)]
pub struct FailingReads {
    pub inner: InMemory,
    /// The status every GET fails with; 0 while none does.
    status: std::sync::atomic::AtomicU16,
    /// GETs that are not HEADs, failed or not.
    reads: AtomicUsize,
}

impl FailingReads {
    /// Store `bytes` under `key`.
    pub fn put(&self, key: &str, bytes: impl Into<Bytes>) {
        futures::executor::block_on(
            self.inner
                .put(&Path::from(key), PutPayload::from(bytes.into())),
        )
        .expect("an in-memory put");
    }

    /// Fail every GET from now on with `status`, or serve them again with
    /// `None`.
    pub fn fail_with(&self, status: Option<u16>) {
        self.status.store(status.unwrap_or(0), Ordering::SeqCst);
    }

    /// How many GETs, HEADs left out, have started.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl std::fmt::Display for FailingReads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FailingReads")
    }
}

/// The server's last answer to a request object_store gave up on, in
/// object_store's wording, which `error::http_status` reads.
#[derive(Debug)]
struct LastAnswer(http::StatusCode);

impl std::fmt::Display for LastAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Server returned non-2xx status code: {}: ", self.0)
    }
}

impl std::error::Error for LastAnswer {}

/// object_store's retry loop giving up on a GET: the request and its
/// retries, with the last answer as its source.
#[derive(Debug)]
struct GaveUp {
    path: String,
    answer: LastAnswer,
}

impl std::fmt::Display for GaveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Error performing GET {} in 1s, after 10 retries, max_retries: 10, \
             retry_timeout: 120s  - {}",
            self.path, self.answer
        )
    }
}

impl std::error::Error for GaveUp {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.answer)
    }
}

#[async_trait::async_trait]
impl ObjectStore for FailingReads {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if options.head {
            return self.inner.get_opts(path, options).await;
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        match self.status.load(Ordering::SeqCst) {
            0 => self.inner.get_opts(path, options).await,
            status => Err(object_store::Error::Generic {
                store: "S3",
                source: Box::new(GaveUp {
                    path: path.to_string(),
                    answer: LastAnswer(
                        http::StatusCode::from_u16(status).expect("an HTTP status code"),
                    ),
                }),
            }),
        }
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// What [`quick_backoff`] returns: the host keeps the shorter times while
/// this lives.
#[derive(Debug)]
pub struct QuickBackoff(#[allow(dead_code)] Arc<crate::throttle::HostThrottle>);

/// Back off from the host of `url` with times a hundredth of the real
/// ones (#1290): its pace from 2.5 ms up to 20 ms, a request it refuses
/// retried after 20 ms. A test of a crate reading a host that answers 429
/// in real time then waits milliseconds where the server would wait
/// seconds; ds-storage's own tests pin the real times on a paused clock.
/// The limit's cut and growth, the quiet interval and `Retry-After` keep
/// theirs. Build the stores on the host while the guard lives: the
/// throttle is the host's for as long as something holds it.
pub fn quick_backoff(url: &str) -> QuickBackoff {
    let url = url::Url::parse(url).expect("an absolute URL");
    let host = crate::throttle::for_url(&url);
    quicken(&host);
    QuickBackoff(host)
}

/// [`quick_backoff`] of the S3 bucket `bucket` at `endpoint` (#1530), as
/// its `503 SlowDown`s back it off: its pace from 2.5 ms up to 20 ms, a
/// refused request retried after 20 ms. The bucket lives while the guard
/// or a store on it does: build the stores while the guard lives, or call
/// it after building one.
pub fn quick_bucket_backoff(endpoint: &str, bucket: &str) -> QuickBackoff {
    let label = crate::traced::s3_label(Some(endpoint), None, bucket);
    let path = crate::bucket::endpoint_path(Some(endpoint));
    let host = crate::bucket::bucket(&label, bucket, &path).throttle;
    quicken(&host);
    QuickBackoff(host)
}

/// Back `host` off on a hundredth of the real times.
fn quicken(host: &crate::throttle::HostThrottle) {
    use crate::throttle::{Timing, PACE_CAP, PACE_FLOOR, RETRY_DELAY};
    host.set_timing(Timing {
        pace_floor: PACE_FLOOR / 100,
        pace_cap: PACE_CAP / 100,
        retry_delay: RETRY_DELAY / 100,
    });
}

/// An S3 bucket's back-off that a test drives (#1530): stores on it
/// ([`Self::store`]) report it through [`crate::DataStore::back_off`],
/// [`Self::slow_down`] backs it off as a `503 SlowDown` the bucket
/// answered would, and [`Self::quiet`] ends it, as the quiet intervals
/// after the last one would. Nothing is sent through it: a store's reads
/// are its inner store's, so a test fails them itself, as
/// [`FailingReads`] does, and reports their `SlowDown`s.
#[derive(Debug, Clone)]
pub struct ThrottledBucket(Arc<crate::throttle::HostThrottle>);

impl ThrottledBucket {
    /// A bucket's back-off, labelled `source` (`host[:port]/bucket`), its
    /// own whatever other test made one of that name.
    pub fn new(source: &str) -> Self {
        let bucket = source.rsplit('/').next().unwrap_or(source);
        Self(Arc::new(crate::throttle::HostThrottle::new(
            source.to_string(),
            crate::throttle::Kind::bucket(bucket, ""),
        )))
    }

    /// A remote store reading `inner` from this bucket.
    pub fn store(&self, inner: Arc<dyn ObjectStore>) -> crate::DataStore {
        let source = crate::traced::Labels {
            store: self.0.host().into(),
            collection: None,
            background: false,
        };
        crate::DataStore::layered(
            inner,
            Some(source),
            Some(self.0.clone()),
            Default::default(),
        )
    }

    /// The bucket throttles: it answers `503 SlowDown` as often as it
    /// takes a bucket to back off (three times), as through a store's
    /// transport, and backs off.
    pub fn slow_down(&self) {
        for request in 0..crate::throttle::BUCKET_TRIGGER as u64 {
            self.0
                .throttled(reqwest::StatusCode::SERVICE_UNAVAILABLE, None, request);
        }
    }

    /// Its back-off is over.
    pub fn quiet(&self) {
        self.0.quiet();
    }
}
