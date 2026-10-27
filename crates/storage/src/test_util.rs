//! Test support for crates that read through a [`DataStore`] (#1055): a
//! store whose GET bodies stall until the test lets them finish, and
//! [`run_stalled`], which runs a synchronous call through such a store on a
//! paused clock and moves the clock [`STALL`] past each stalled GET. A test
//! pins with them which [`FetchBudget`] a caller passes: the stall outlasts
//! [`FetchBudget::Request`], not [`FetchBudget::Background`], so a read under
//! the wrong budget fails or succeeds where the test expects the other.
//!
//! Compiled for ds-storage's own tests and, through its `test-util`
//! feature, for the tests of a crate that enables the feature on its
//! ds-storage dev-dependency. Never in a server build.
//!
//! [`DataStore`]: crate::DataStore

use std::future::Future;
use std::pin::Pin;
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
    use crate::throttle::{Timing, PACE_CAP, PACE_FLOOR, RETRY_DELAY};
    let url = url::Url::parse(url).expect("an absolute URL");
    let host = crate::throttle::for_url(&url);
    host.set_timing(Timing {
        pace_floor: PACE_FLOOR / 100,
        pace_cap: PACE_CAP / 100,
        retry_delay: RETRY_DELAY / 100,
    });
    QuickBackoff(host)
}
