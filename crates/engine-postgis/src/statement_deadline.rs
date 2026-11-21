//! Statements end at the request deadline (#1500).
//!
//! An EDR query, a Features page of events and a WMS GetFeatureInfo pick
//! run on the shared query runtime under the request's absolute deadline,
//! [`ds_core::deadline::current`]. [`Checkouts::block_on`] runs an engine
//! call's statements on that thread (`block_in_place` over its runtime) and
//! races them against the deadline. Past it, the call answers
//! [`DataServerError::DeadlineExceeded`], logged once at WARN with the SQL
//! templates of the statements it stopped (never their binds), and the
//! database is told to stop them too. Before, only the role's
//! `statement_timeout` ended them, and the worker waited for it.
//!
//! A cancel request names a backend, not a statement, so:
//!
//! - Each connection's cancel token is kept from its checkout
//!   ([`Checkouts::get`]) until the call gives the connection back
//!   ([`Conn`]'s drop), and only the tokens still out at the deadline are
//!   used ([`Held`]). A connection back in the pool may be running another
//!   request's statement, which its cancel would stop.
//! - A connection whose token was used is discarded, never recycled: the
//!   cancel can arrive after its statement ended and stop the next one.
//!   Its socket closes, and its backend ends once the cancel reaches it.
//!
//! The cancels go out as tasks on the calling runtime, each bounded by
//! [`CANCEL_TIMEOUT`], so an unreachable server never holds the worker. On
//! the happy path a checkout only copies the connection's cancel token: no
//! round trip. Without a deadline nothing is tracked, and a call runs to
//! its end as before.
//!
//! The map layers' strike-window and station-frame reads (`maps.rs`,
//! `station_map.rs`) do not come here. They fill caches other renders
//! join, and a fill that outlives the render that started it still serves
//! the retry; stopping it at that render's deadline would leave a frame
//! slower than the deadline never drawn.

use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use deadpool_postgres::{Object, Pool};
use ds_core::error::DataServerError;
use tokio_postgres::{CancelToken, NoTls, Row};

use crate::engine::map_pg_error;
use crate::query::{params_as_refs, BuiltQuery};

/// How long one cancel request may take to reach the server: it opens a
/// connection of its own, which a dead server would hold.
pub(crate) const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

/// The pooled connections one engine call checks out, under the calling
/// thread's request deadline if it runs under one. Build it on the thread
/// that runs the call ([`Self::current`]), and run the call's statements in
/// [`Self::block_on`].
pub(crate) struct Checkouts {
    /// `None` without a deadline: nothing is tracked.
    tracker: Option<Arc<Tracker>>,
}

/// One call's deadline and the connections it holds.
struct Tracker {
    deadline: Instant,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    held: Held<CancelToken>,
    stopped: Stopped,
}

/// The cancel tokens of the connections a call holds, from checkout until
/// it gives each back. Only a token still out is ever used: after
/// [`Self::expire`] took it, its connection is discarded when given back.
#[derive(Debug)]
struct Held<T> {
    next: u64,
    out: HashMap<u64, T>,
    expired: bool,
}

impl<T> Default for Held<T> {
    fn default() -> Self {
        Self {
            next: 0,
            out: HashMap::new(),
            expired: false,
        }
    }
}

impl<T> Held<T> {
    /// A connection checked out, holding `token`: its id for
    /// [`Self::give_back`]. One checked out after the deadline is never
    /// held, so it is discarded too.
    fn hold(&mut self, token: T) -> u64 {
        let id = self.next;
        self.next += 1;
        if !self.expired {
            self.out.insert(id, token);
        }
        id
    }

    /// Connection `id` given back: `true` when it may return to the pool,
    /// `false` when the deadline took its token and it must be discarded.
    fn give_back(&mut self, id: u64) -> bool {
        self.out.remove(&id).is_some()
    }

    /// The deadline: the tokens of every connection still out, to cancel.
    /// Each of those connections is discarded when given back.
    fn expire(&mut self) -> Vec<T> {
        self.expired = true;
        self.out.drain().map(|(_, token)| token).collect()
    }
}

/// What a call was doing when its deadline passed, for its log line.
#[derive(Debug, Default, PartialEq)]
struct Stopped {
    statements: usize,
    connection_waits: usize,
    /// The distinct SQL templates of `statements`, in the order stopped.
    templates: Vec<String>,
}

/// What an [`InFlight`] guard watches.
#[derive(Debug, Clone, Copy)]
enum Doing<'a> {
    /// Waiting for a pooled connection.
    Connection,
    /// A statement running, by its SQL template.
    Statement(&'a str),
}

/// Records what it watches as stopped when the deadline drops it before
/// [`Self::finish`]. Dropped for any other reason (a sibling's error ends
/// a fan-out), it records nothing.
struct InFlight<'a> {
    tracker: Option<&'a Tracker>,
    doing: Doing<'a>,
}

impl<'a> InFlight<'a> {
    fn new(tracker: Option<&'a Tracker>, doing: Doing<'a>) -> Self {
        Self { tracker, doing }
    }

    fn finish(mut self) {
        self.tracker = None;
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if let Some(tracker) = self.tracker {
            tracker.stopped(self.doing);
        }
    }
}

impl Tracker {
    fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            state: Mutex::default(),
        }
    }

    /// The state, also after a panic elsewhere: a connection's drop must
    /// still decide whether it goes back to the pool.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn hold(&self, token: CancelToken) -> u64 {
        self.lock().held.hold(token)
    }

    fn give_back(&self, id: u64) -> bool {
        self.lock().held.give_back(id)
    }

    fn expire(&self) -> Vec<CancelToken> {
        self.lock().held.expire()
    }

    fn stopped(&self, doing: Doing<'_>) {
        let mut state = self.lock();
        if !state.held.expired {
            return;
        }
        let stopped = &mut state.stopped;
        match doing {
            Doing::Connection => stopped.connection_waits += 1,
            Doing::Statement(sql) => {
                stopped.statements += 1;
                if !stopped.templates.iter().any(|t| t == sql) {
                    stopped.templates.push(sql.to_string());
                }
            }
        }
    }

    /// The deadline's one WARN: the templates of the statements it stopped,
    /// never their binds.
    fn log(&self, canceled: usize) {
        let state = self.lock();
        let stopped = &state.stopped;
        if stopped.statements > 0 {
            tracing::warn!(
                sql = %stopped.templates.join("\n;\n"),
                statements = stopped.statements,
                canceled,
                "postgis statements stopped at the request deadline; the database is told to cancel them"
            );
        } else {
            tracing::warn!(
                connection_waits = stopped.connection_waits,
                canceled,
                "postgis query reached the request deadline with no statement running"
            );
        }
    }
}

impl Checkouts {
    /// The calling thread's request deadline, if it runs under one.
    pub(crate) fn current() -> Self {
        Self::until(ds_core::deadline::current())
    }

    fn until(deadline: Option<Instant>) -> Self {
        Self {
            tracker: deadline.map(|deadline| Arc::new(Tracker::new(deadline))),
        }
    }

    /// A pooled connection for this call. A failed checkout is the opaque
    /// 500, as before; the deadline ends a wait for one.
    pub(crate) async fn get(&self, pool: &Pool) -> Result<Conn, DataServerError> {
        let waiting = InFlight::new(self.tracker.as_deref(), Doing::Connection);
        let object = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        waiting.finish();
        let slot = self.tracker.as_ref().map(|tracker| {
            let token = object.cancel_token();
            (Arc::clone(tracker), tracker.hold(token))
        });
        Ok(Conn {
            object: Some(object),
            slot,
        })
    }

    /// Run `work`, the call's statements, on this thread to its end or the
    /// deadline (module docs). Without a deadline, to its end.
    pub(crate) fn block_on<T>(
        &self,
        work: impl Future<Output = Result<T, DataServerError>>,
    ) -> Result<T, DataServerError> {
        let Some(tracker) = &self.tracker else {
            return block_on_async(work);
        };
        if Instant::now() >= tracker.deadline {
            tracker.log(0);
            return Err(DataServerError::DeadlineExceeded);
        }
        let runtime = tokio::runtime::Handle::try_current().ok();
        block_on_async(async {
            let tokens = {
                let work = std::pin::pin!(work);
                if let Ok(result) = tokio::time::timeout_at(tracker.deadline.into(), work).await {
                    return result;
                }
                // Taken before `work` drops at the end of this block, so
                // every connection it still holds is discarded, not
                // recycled, and every statement it stopped is recorded.
                tracker.expire()
            };
            tracker.log(tokens.len());
            cancel(tokens, runtime.as_ref()).await;
            Err(DataServerError::DeadlineExceeded)
        })
    }
}

/// A pooled connection a call holds: back to the pool when dropped, unless
/// the call's deadline canceled it, which discards it.
pub(crate) struct Conn {
    object: Option<Object>,
    /// The call's tracker and this connection's id there; `None` without a
    /// deadline.
    slot: Option<(Arc<Tracker>, u64)>,
}

impl Deref for Conn {
    type Target = Object;

    fn deref(&self) -> &Object {
        self.object.as_ref().expect("a held connection")
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let Some(object) = self.object.take() else {
            return;
        };
        if let Some((tracker, id)) = &self.slot {
            if !tracker.give_back(*id) {
                // Its cancel is on the way: never back to the pool. Dropping
                // the wrapper ends its connection task and closes the socket.
                drop(Object::take(object));
                return;
            }
        }
        drop(object);
    }
}

impl Conn {
    /// `statement`, a future running `q` on this connection, watched so the
    /// deadline's log names `q`'s template if it stops it.
    pub(crate) async fn run<T>(&self, q: &BuiltQuery, statement: impl Future<Output = T>) -> T {
        let running = InFlight::new(
            self.slot.as_ref().map(|(tracker, _)| &**tracker),
            Doing::Statement(&q.sql),
        );
        let out = statement.await;
        running.finish();
        out
    }

    /// `q`'s rows; a database error through [`map_pg_error`].
    pub(crate) async fn rows(&self, q: &BuiltQuery) -> Result<Vec<Row>, DataServerError> {
        self.run(q, self.object().query(&q.sql, &params_as_refs(&q.params)))
            .await
            .map_err(|e| map_pg_error(e, q))
    }

    /// `q`'s row, if any; a database error through [`map_pg_error`].
    pub(crate) async fn row_opt(&self, q: &BuiltQuery) -> Result<Option<Row>, DataServerError> {
        self.run(
            q,
            self.object().query_opt(&q.sql, &params_as_refs(&q.params)),
        )
        .await
        .map_err(|e| map_pg_error(e, q))
    }

    fn object(&self) -> &Object {
        self
    }
}

/// Send each cancel as a task on `runtime`, so the worker returns at once;
/// outside any runtime (the throwaway one of [`block_on_async`] would drop
/// the tasks) wait for them, each bounded by [`CANCEL_TIMEOUT`].
async fn cancel(tokens: Vec<CancelToken>, runtime: Option<&tokio::runtime::Handle>) {
    let sends = tokens.into_iter().map(|token| async move {
        match tokio::time::timeout(CANCEL_TIMEOUT, token.cancel_query(NoTls)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(
                error = %e,
                "postgis: a cancel request at the request deadline failed"
            ),
            Err(_) => tracing::warn!(
                timeout_secs = CANCEL_TIMEOUT.as_secs(),
                "postgis: a cancel request at the request deadline timed out"
            ),
        }
    });
    match runtime {
        Some(runtime) => {
            for send in sends {
                drop(runtime.spawn(send));
            }
        }
        None => {
            futures::future::join_all(sends).await;
        }
    }
}

/// The sync ↔ async bridge: `block_in_place` over the calling runtime,
/// which the query runtime's workers and `spawn_blocking` threads allow.
fn block_on_async<F, T>(fut: F) -> T
where
    F: Future<Output = T>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            // No current runtime — spin up a temporary one. Used in unit
            // tests outside tokio, never in production where the query
            // runtime always provides one.
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime")
                .block_on(fut)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Sets its flag when dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Only a connection still out at the deadline is canceled, and only
    /// such a connection is discarded: one given back first goes to the
    /// pool and its token is never used, since another request may be
    /// running a statement on it by then.
    #[test]
    fn only_connections_still_held_are_canceled_and_discarded() {
        let mut held = Held::default();
        let returned = held.hold("returned");
        let running = held.hold("running");
        let idle = held.hold("idle");
        assert!(held.give_back(returned), "back to the pool");
        let mut canceled = held.expire();
        canceled.sort();
        assert_eq!(canceled, ["idle", "running"]);
        assert!(!held.give_back(running), "discarded");
        assert!(!held.give_back(idle), "discarded");
        // Checked out after the deadline: discarded, never canceled.
        let late = held.hold("late");
        assert!(held.expire().is_empty());
        assert!(!held.give_back(late));
    }

    #[test]
    fn without_a_deadline_a_call_runs_to_its_end() {
        let checkouts = Checkouts::until(None);
        let result = checkouts.block_on(async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok(7)
        });
        assert_eq!(result.unwrap(), 7);
        assert!(checkouts.tracker.is_none(), "nothing is tracked");
    }

    #[test]
    fn a_call_done_before_its_deadline_answers() {
        let checkouts = Checkouts::until(Some(Instant::now() + Duration::from_secs(30)));
        let result = checkouts.block_on(async {
            let tracker = checkouts.tracker.as_deref();
            let running = InFlight::new(tracker, Doing::Statement("SELECT 1"));
            tokio::time::sleep(Duration::from_millis(10)).await;
            running.finish();
            Ok(7)
        });
        assert_eq!(result.unwrap(), 7);
        let tracker = checkouts.tracker.as_deref().unwrap();
        assert!(!tracker.lock().held.expired);
        assert_eq!(tracker.lock().stopped, Stopped::default());
    }

    /// At the deadline the call is `DeadlineExceeded` at once, its work
    /// dropped before it returns, with the statement it stopped recorded
    /// for the log and a statement a sibling ended earlier left out.
    #[test]
    fn the_deadline_ends_a_call_and_names_what_it_stopped() {
        let checkouts = Checkouts::until(Some(Instant::now() + Duration::from_millis(50)));
        let dropped = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let result: Result<(), _> = checkouts.block_on(async {
            let tracker = checkouts.tracker.as_deref();
            let _flag = DropFlag(Arc::clone(&dropped));
            // Dropped before the deadline, as a sibling's error drops one.
            drop(InFlight::new(tracker, Doing::Statement("SELECT early")));
            let _running = InFlight::new(tracker, Doing::Statement("SELECT pg_sleep(60)"));
            let _again = InFlight::new(tracker, Doing::Statement("SELECT pg_sleep(60)"));
            let _waiting = InFlight::new(tracker, Doing::Connection);
            std::future::pending().await
        });
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(dropped.load(Ordering::SeqCst), "the work is dropped");
        let tracker = checkouts.tracker.as_deref().unwrap();
        assert_eq!(
            tracker.lock().stopped,
            Stopped {
                statements: 2,
                connection_waits: 1,
                templates: vec!["SELECT pg_sleep(60)".into()],
            }
        );
    }

    /// A call that starts past its deadline never runs its work.
    #[test]
    fn a_call_past_its_deadline_does_not_start() {
        let checkouts = Checkouts::until(Some(Instant::now()));
        let polled = Arc::new(AtomicBool::new(false));
        let result: Result<(), _> = checkouts.block_on({
            let polled = Arc::clone(&polled);
            async move {
                polled.store(true, Ordering::SeqCst);
                Ok(())
            }
        });
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
        assert!(!polled.load(Ordering::SeqCst));
    }

    /// On the query runtime the call returns at the deadline: the bridge
    /// is `block_in_place` over the worker's runtime, which keeps driving
    /// the timer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_deadline_ends_a_call_on_a_runtime_worker() {
        let started = Instant::now();
        let result: Result<(), _> = tokio::spawn(async {
            let _deadline =
                ds_core::deadline::enter(Some(Instant::now() + Duration::from_millis(50)));
            Checkouts::current().block_on(std::future::pending())
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
