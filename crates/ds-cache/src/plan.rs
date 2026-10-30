//! The process-wide registry of caches and admission budgets: the memory plan
//! (#1306).
//!
//! Every byte-bounded cache and every admission budget enters here when it
//! is built: an [`Entry`] naming it by its `/metrics` family, who it serves
//! ([`Scope`]), where its size came from ([`SizeSource`]), and a way to read
//! its [`Usage`]. The server lists the entries (`/admin/cache-plan`, a log
//! block at boot and on reload, `cache_size_source` on `/metrics`), so an
//! operator sees every size and its knob in one place. A budget registered
//! with [`register_budget`] also hands over its high-water mark
//! ([`take_peaks`], #1307).
//!
//! Entries are weak: an entry reads its owner through a `Weak`, and one
//! whose owner was dropped (a rebuilt or removed collection's engine) leaves
//! the plan. Registering the same owner under the same id again replaces
//! the entry, so a cache kept across a reload is listed once.
//!
//! Each [`snapshot`] also samples every cache's counters, at most every
//! [`SAMPLE_EVERY`], so a row carries what its cache did over the last
//! [`WINDOW`]: its [`Activity`], with a [`Label`] saying whether it needs
//! more memory (#1308).
//!
//! Engines publish what each cache must hold for their data ([`demand`],
//! #1309): a row carries its cache's [`Demand`], from which the plan
//! suggests a size and says when the configured one is short.

use std::any::Any;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::{CacheMetrics, Churn, MIB};

pub mod demand;
pub use demand::{check_rows, CacheDemand, Demand, DemandSlot, Shortfall, DEMAND_FIELDS};

/// A cache holds entries for reuse; a budget admits transient work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Cache,
    Budget,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Cache => "cache",
            Kind::Budget => "budget",
        }
    }
}

/// Who an entry serves, and so how many of it a process has.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// One per process, shared by every collection: the render output
    /// caches, the render and EDR response budgets.
    Process,
    /// One per process, used by the collections of one engine type.
    Engine(&'static str),
    /// One per collection of an engine type.
    Collection { engine: &'static str, id: String },
    /// One per data source, shared by the collections that read it: a GRIB
    /// source and its level collections, `id` the source's collection.
    Source { engine: &'static str, id: String },
}

impl Scope {
    /// `process`, `engine`, `collection` or `source`.
    pub fn kind(&self) -> &'static str {
        match self {
            Scope::Process => "process",
            Scope::Engine(_) => "engine",
            Scope::Collection { .. } => "collection",
            Scope::Source { .. } => "source",
        }
    }

    /// The engine type, for every scope but [`Scope::Process`].
    pub fn engine(&self) -> Option<&'static str> {
        match self {
            Scope::Process => None,
            Scope::Engine(engine)
            | Scope::Collection { engine, .. }
            | Scope::Source { engine, .. } => Some(engine),
        }
    }

    /// The collection id of a [`Scope::Collection`] or [`Scope::Source`]:
    /// the `collection` label of its `/metrics` family.
    pub fn collection(&self) -> Option<&str> {
        match self {
            Scope::Collection { id, .. } | Scope::Source { id, .. } => Some(id),
            Scope::Process | Scope::Engine(_) => None,
        }
    }
}

/// Where a size came from, in the words `/admin/cache-plan` and the
/// `cache_size_source` metric use: `default`, `env:MC_…`,
/// `config:[server] …`, `collection:[grib] …`, `derived:[grib] …` or
/// `constant`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SizeSource {
    /// The built-in default: nothing set this size.
    Default,
    /// An environment variable.
    Env(&'static str),
    /// A server-wide config key, e.g. `[server] metatile_cache_mb`.
    Config(&'static str),
    /// A collection's config key, e.g. `[grib] grid_cache_mb`.
    Collection(&'static str),
    /// Computed from another collection key's size, this one unset: the
    /// GRIB point-index cache from `[grib] message_cache_mb` (#1333).
    Derived(&'static str),
    /// Fixed in code, with no setting.
    Constant,
}

impl SizeSource {
    /// `source` when a config key's value differs from its built-in default,
    /// else [`SizeSource::Default`]: the one place a size source is
    /// inferred from a value. The keys it serves have serde defaults and
    /// keep no record of having been set, so an explicit value equal to the
    /// default reports as `default` until #1313 makes them `Option<u64>`;
    /// then each caller names its key when it is `Some`, and this goes.
    pub fn unless_default(value: u64, default: u64, source: SizeSource) -> SizeSource {
        if value == default {
            SizeSource::Default
        } else {
            source
        }
    }
}

impl fmt::Display for SizeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SizeSource::Default => f.write_str("default"),
            SizeSource::Env(var) => write!(f, "env:{var}"),
            SizeSource::Config(key) => write!(f, "config:{key}"),
            SizeSource::Collection(key) => write!(f, "collection:{key}"),
            SizeSource::Derived(key) => write!(f, "derived:{key}"),
            SizeSource::Constant => f.write_str("constant"),
        }
    }
}

/// A size in MB from the environment and where it came from: `var` when it
/// is set to a whole number (trimmed), else `default_mb` and
/// [`SizeSource::Default`]. `0` conventionally disables a cache.
pub fn env_size(var: &'static str, default_mb: u64) -> (u64, SizeSource) {
    match crate::parse_env_mb(var) {
        Some(mb) => (mb, SizeSource::Env(var)),
        None => (default_mb, SizeSource::Default),
    }
}

/// [`env_size`] in bytes.
pub fn env_bytes(var: &'static str, default_mb: u64) -> (u64, SizeSource) {
    let (mb, source) = env_size(var, default_mb);
    (mb.saturating_mul(MIB), source)
}

/// What one cache or budget holds now. Hits, misses and churn are
/// cumulative since it was built; a budget has no entries, hits, misses or
/// churn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// The configured size; `0` for a disabled cache.
    pub capacity_bytes: u64,
    /// Resident bytes of a cache, reserved bytes of a budget.
    pub used_bytes: u64,
    pub entries: Option<u64>,
    pub hits: Option<u64>,
    pub misses: Option<u64>,
    /// What the cache admitted, refused and evicted (#1308).
    pub churn: Option<Churn>,
}

impl Usage {
    /// A cache's `/metrics` snapshot, with its entry count when known.
    pub fn cache(metrics: CacheMetrics, entries: Option<u64>) -> Self {
        Usage {
            capacity_bytes: metrics.capacity_bytes,
            used_bytes: metrics.bytes,
            entries,
            hits: Some(metrics.hits),
            misses: Some(metrics.misses),
            churn: Some(metrics.churn),
        }
    }

    /// A budget's capacity and current reservations.
    pub fn budget(capacity_bytes: u64, used_bytes: u64) -> Self {
        Usage {
            capacity_bytes,
            used_bytes,
            ..Usage::default()
        }
    }
}

/// One cache or budget in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The `/metrics` family: `metatile_cache` for `metatile_cache_bytes`,
    /// `geotiff_decode` for `geotiff_decode_capacity_bytes`.
    pub id: &'static str,
    pub kind: Kind,
    pub scope: Scope,
    pub source: SizeSource,
}

impl Entry {
    pub fn cache(id: &'static str, scope: Scope, source: SizeSource) -> Self {
        Entry {
            id,
            kind: Kind::Cache,
            scope,
            source,
        }
    }

    pub fn budget(id: &'static str, scope: Scope, source: SizeSource) -> Self {
        Entry {
            id,
            kind: Kind::Budget,
            scope,
            source,
        }
    }
}

/// A registered entry and what it holds at [`snapshot`] time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub entry: Entry,
    pub usage: Usage,
    /// What a cache did over the last [`WINDOW`], or since it was first
    /// sampled when that is less, but at least [`SAMPLE_EVERY`]. `None`
    /// for a budget, and for a cache sampled for less than that.
    pub window: Option<Activity>,
    /// What the cache must hold for its collections' data, as their
    /// engines last published it (#1309). `None` for a budget, and for a
    /// cache no engine publishes a demand for.
    pub demand: Option<Demand>,
}

/// How often [`snapshot`] samples a cache's counters, at most. `/metrics`
/// takes a snapshot on every scrape, so the samples are this far apart
/// whenever something scrapes; without a scraper, the window is the time
/// since the previous plan read.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(300);

/// The span of a row's [`Activity`] once its cache has been sampled for
/// that long: up to [`SAMPLE_EVERY`] more, as samples fall.
pub const WINDOW: Duration = Duration::from_secs(3600);

/// A cache's counters as sampled, oldest first.
#[derive(Default)]
struct History(VecDeque<(Instant, Usage)>);

impl History {
    /// Sample `usage` at `now` if the last sample is [`SAMPLE_EVERY`] old,
    /// keep the newest sample at least [`WINDOW`] old and those after it,
    /// and return what the cache did since the oldest kept.
    fn observe(&mut self, now: Instant, usage: &Usage) -> Option<Activity> {
        usage.churn?;
        let went_back = self
            .0
            .back()
            .is_some_and(|(_, last)| Activity::between(last, usage, 0).is_none());
        if went_back {
            self.0.clear();
        }
        let due = self
            .0
            .back()
            .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= SAMPLE_EVERY);
        if due {
            self.0.push_back((now, *usage));
        }
        while self.0.len() > 1 && now.saturating_duration_since(self.0[1].0) >= WINDOW {
            self.0.pop_front();
        }
        let (start, earlier) = self.0.front()?;
        let span = now.saturating_duration_since(*start);
        if span < SAMPLE_EVERY {
            return None;
        }
        Activity::between(earlier, usage, span.as_secs())
    }
}

/// What a cache did over a span of time: the differences of its counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    pub secs: u64,
    /// The capacity at the end of the span.
    pub capacity_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub churn: Churn,
}

/// Fewer lookups an hour than this: no demand yet, so fill and hit ratio
/// say nothing ([`Label::Idle`]). Like the thresholds below, a starting
/// point to be set from the first two weeks of production data (#1308).
pub const IDLE_LOOKUPS_PER_HOUR: f64 = 60.0;
/// Turnover above this many capacities an hour churns: thrashing or a scan.
pub const CHURN_TURNOVER_PER_HOUR: f64 = 1.0;
/// Ghost hits within 1 × capacity at least this share of misses: thrashing.
pub const THRASHING_GHOST_SHARE: f64 = 0.20;
/// Ghost hits within 2 × capacity, the deepest the ring sees, under this
/// share of misses: a scan.
pub const SCAN_GHOST_SHARE: f64 = 0.05;

/// Whether a cache needs memory, from its [`Activity`] (#1308).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    /// Fewer than [`IDLE_LOOKUPS_PER_HOUR`] lookups: no demand yet, so fill
    /// and hit ratio say nothing.
    Idle,
    /// Turnover above [`CHURN_TURNOVER_PER_HOUR`], and ghost hits within
    /// 1 × capacity at least [`THRASHING_GHOST_SHARE`] of misses: evicted
    /// entries are asked for again soon after. More memory helps.
    Thrashing,
    /// Turnover above [`CHURN_TURNOVER_PER_HOUR`], and ghost hits under
    /// [`SCAN_GHOST_SHARE`] of misses: a one-pass read evicts entries nobody
    /// asks for again. The fix is a bypass or scan resistance, not memory.
    Scan,
    /// Turnover above [`CHURN_TURNOVER_PER_HOUR`] in a cache without a
    /// ghost ring, which cannot tell thrashing from a scan. To tell, build
    /// that cache [`crate::ByteBoundedCache::with_ghosts`].
    Churning,
    /// Everything else: it can lend memory above its floor.
    Healthy,
}

impl Label {
    pub fn as_str(self) -> &'static str {
        match self {
            Label::Idle => "idle",
            Label::Thrashing => "thrashing",
            Label::Scan => "scan",
            Label::Churning => "churning",
            Label::Healthy => "healthy",
        }
    }
}

impl Activity {
    /// The counters of `end` less those of `start`, the same cache sampled
    /// `secs` apart; `None` for a budget, or if a counter went back (the
    /// cache was rebuilt).
    pub fn between(start: &Usage, end: &Usage, secs: u64) -> Option<Self> {
        Some(Activity {
            secs,
            capacity_bytes: end.capacity_bytes,
            hits: end.hits?.checked_sub(start.hits?)?,
            misses: end.misses?.checked_sub(start.misses?)?,
            churn: end.churn?.since(&start.churn?)?,
        })
    }

    /// Hits and misses. A bypass is neither.
    pub fn lookups(&self) -> u64 {
        self.hits.saturating_add(self.misses)
    }

    /// `count` over the span, per hour.
    pub fn per_hour(&self, count: u64) -> f64 {
        count as f64 * 3600.0 / self.secs.max(1) as f64
    }

    pub fn hit_ratio(&self) -> Option<f64> {
        let lookups = self.lookups();
        (lookups > 0).then(|| self.hits as f64 / lookups as f64)
    }

    /// Capacities admitted per hour; `None` for a disabled cache.
    pub fn turnover_per_hour(&self) -> Option<f64> {
        (self.capacity_bytes > 0)
            .then(|| self.per_hour(self.churn.inserted_bytes) / self.capacity_bytes as f64)
    }

    /// Ghost hits at each of [`crate::GHOST_DEPTHS`] as a share of misses;
    /// `None` without a ghost ring or without misses.
    pub fn ghost_share(&self) -> Option<[f64; 3]> {
        let ghost_hits = self.churn.ghost_hits?;
        (self.misses > 0).then(|| ghost_hits.map(|hits| hits as f64 / self.misses as f64))
    }

    /// The hit ratio a cache 1.5 ×, 2 × and 3 × as large would have had:
    /// hits plus the ghost hits within 0.5 ×, 1 × and 2 × the capacity, over
    /// lookups. `None` without a ghost ring or without lookups.
    pub fn estimated_hit_ratios(&self) -> Option<[f64; 3]> {
        let ghost_hits = self.churn.ghost_hits?;
        let lookups = self.lookups();
        (lookups > 0).then(|| ghost_hits.map(|ghosts| (self.hits + ghosts) as f64 / lookups as f64))
    }

    /// The cache's [`Label`]; `None` for a disabled cache that has demand.
    pub fn label(&self) -> Option<Label> {
        if self.per_hour(self.lookups()) < IDLE_LOOKUPS_PER_HOUR {
            return Some(Label::Idle);
        }
        if self.turnover_per_hour()? <= CHURN_TURNOVER_PER_HOUR || self.misses == 0 {
            return Some(Label::Healthy);
        }
        let Some(share) = self.ghost_share() else {
            return Some(Label::Churning);
        };
        Some(if share[1] >= THRASHING_GHOST_SHARE {
            Label::Thrashing
        } else if share[2] < SCAN_GHOST_SHARE {
            Label::Scan
        } else {
            Label::Healthy
        })
    }
}

/// Reads an owner's figure while it lives.
type Reader<V> = Box<dyn Fn() -> Option<V> + Send + Sync>;

struct Registered {
    seq: u64,
    entry: Entry,
    /// The owner's address, so registering it again under the same id
    /// replaces this entry.
    owner_addr: usize,
    owner: Weak<dyn Any + Send + Sync>,
    read: Reader<Usage>,
    /// A budget's high-water mark, taken and restarted ([`take_peaks`]).
    take_peak: Option<Reader<u64>>,
    /// Shared with the entry this one replaced, if any: the same cache.
    history: Arc<Mutex<History>>,
}

static REGISTRY: Mutex<Vec<Arc<Registered>>> = Mutex::new(Vec::new());
static NEXT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Enter `owner` in the plan as `entry`, read by `usage` while `owner` is
/// alive. Registering the same owner under the same id again replaces the
/// entry (a reload that keeps a cache, or a changed size source).
pub fn register<T: Send + Sync + 'static>(entry: Entry, owner: &Arc<T>, usage: fn(&T) -> Usage) {
    insert(entry, owner, usage, None);
}

/// [`register`] a budget whose high-water mark `take_peak` returns: the
/// most it held at once since the previous call, which restarts the mark
/// from what it holds now (#1307).
pub fn register_budget<T: Send + Sync + 'static>(
    entry: Entry,
    owner: &Arc<T>,
    usage: fn(&T) -> Usage,
    take_peak: fn(&T) -> u64,
) {
    insert(entry, owner, usage, Some(take_peak));
}

fn insert<T: Send + Sync + 'static>(
    entry: Entry,
    owner: &Arc<T>,
    usage: fn(&T) -> Usage,
    take_peak: Option<fn(&T) -> u64>,
) {
    let weak = Arc::downgrade(owner);
    let owner_addr = Arc::as_ptr(owner).cast::<()>() as usize;
    let any: Weak<dyn Any + Send + Sync> = weak.clone();
    let take_peak = take_peak.map(|take_peak| {
        let weak = weak.clone();
        Box::new(move || weak.upgrade().map(|owner| take_peak(&owner))) as Reader<u64>
    });
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let same = |r: &Registered| r.owner_addr == owner_addr && r.entry.id == entry.id;
    let history = registry
        .iter()
        .find(|r| r.owner.strong_count() > 0 && same(r))
        .map(|r| Arc::clone(&r.history))
        .unwrap_or_default();
    registry.retain(|r| r.owner.strong_count() > 0 && !same(r));
    registry.push(Arc::new(Registered {
        seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
        owner_addr,
        owner: any,
        read: Box::new(move || weak.upgrade().map(|owner| usage(&owner))),
        take_peak,
        entry,
        history,
    }));
}

/// Every live entry with its usage, caches before budgets, then by scope
/// (process, engine, collection, source) and id. Where two live entries
/// share an id and scope (an engine replaced by a reload, still held by a
/// request that started before it), the one registered last is listed.
/// Samples each cache for its [`Row::window`].
pub fn snapshot() -> Vec<Row> {
    snapshot_at(Instant::now())
}

/// [`snapshot`] as of `now`.
fn snapshot_at(now: Instant) -> Vec<Row> {
    let registered: Vec<Arc<Registered>> = {
        let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        registry.retain(|r| r.owner.strong_count() > 0);
        registry.clone()
    };
    let demands = demand::demands();
    // Read outside the lock: a cache's usage takes its own shard locks.
    let mut rows: Vec<(u64, Row)> = registered
        .iter()
        .filter_map(|r| {
            let usage = (r.read)()?;
            let window = r
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .observe(now, &usage);
            let demand = match r.entry.kind {
                Kind::Cache => demands.get(&(r.entry.id, r.entry.scope.clone())).cloned(),
                Kind::Budget => None,
            };
            Some((
                r.seq,
                Row {
                    entry: r.entry.clone(),
                    usage,
                    window,
                    demand,
                },
            ))
        })
        .collect();
    rows.sort_by(|(a_seq, a), (b_seq, b)| {
        (a.entry.kind, &a.entry.scope, a.entry.id)
            .cmp(&(b.entry.kind, &b.entry.scope, b.entry.id))
            .then(b_seq.cmp(a_seq))
    });
    rows.dedup_by(|(_, later), (_, kept)| {
        later.entry.id == kept.entry.id && later.entry.scope == kept.entry.scope
    });
    rows.into_iter().map(|(_, row)| row).collect()
}

/// Every live budget registered with a high-water mark
/// ([`register_budget`]) and the most it held at once since the previous
/// call, which restarts each mark: `/metrics` takes them once per scrape,
/// so the marks are per scrape interval (#1307). Where two live entries
/// share an id and scope, the one registered last is taken, as
/// [`snapshot`] lists it.
pub fn take_peaks() -> Vec<(Entry, u64)> {
    let mut registered: Vec<Arc<Registered>> = {
        let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        registry.retain(|r| r.owner.strong_count() > 0);
        registry
            .iter()
            .filter(|r| r.take_peak.is_some())
            .cloned()
            .collect()
    };
    registered.sort_by(|a, b| {
        (a.entry.id, &a.entry.scope)
            .cmp(&(b.entry.id, &b.entry.scope))
            .then(b.seq.cmp(&a.seq))
    });
    registered.dedup_by(|later, kept| {
        later.entry.id == kept.entry.id && later.entry.scope == kept.entry.scope
    });
    registered
        .iter()
        .filter_map(|r| Some((r.entry.clone(), (r.take_peak.as_ref()?)()?)))
        .collect()
}

/// Serializes the tests that read the registry. A [`snapshot`] holds every
/// live owner while it reads its usage, so one running in a parallel test
/// keeps an owner a test has just dropped alive that long, and the test
/// then still finds it listed.
#[cfg(test)]
pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(u64);

    fn usage(fixed: &Fixed) -> Usage {
        Usage::budget(fixed.0, 0)
    }

    /// Rows of this test's own ids: tests share the process-wide registry.
    /// Take [`serial`] first.
    fn rows(prefix: &str) -> Vec<Row> {
        snapshot()
            .into_iter()
            .filter(|row| row.entry.id.starts_with(prefix))
            .collect()
    }

    #[test]
    fn a_dropped_owner_leaves_the_plan() {
        let _serial = serial();
        let owner = Arc::new(Fixed(7));
        register(
            Entry::budget("plan_test_dropped", Scope::Process, SizeSource::Constant),
            &owner,
            usage,
        );
        assert_eq!(rows("plan_test_dropped").len(), 1);
        drop(owner);
        assert!(rows("plan_test_dropped").is_empty());
    }

    #[test]
    fn registering_an_owner_again_replaces_its_entry() {
        let _serial = serial();
        let owner = Arc::new(Fixed(7));
        for source in [SizeSource::Default, SizeSource::Env("MC_PLAN_TEST_MB")] {
            register(
                Entry::cache("plan_test_again", Scope::Engine("grib"), source),
                &owner,
                usage,
            );
        }
        let rows = rows("plan_test_again");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.source, SizeSource::Env("MC_PLAN_TEST_MB"));
        assert_eq!(rows[0].usage.capacity_bytes, 7);
    }

    /// Two live owners of one id and scope: a replaced engine still held by
    /// a request. The newer one is listed; other scopes are not affected.
    #[test]
    fn the_newest_of_two_live_owners_is_listed() {
        let _serial = serial();
        let scope = |id: &str| Scope::Collection {
            engine: "geotiff",
            id: id.to_string(),
        };
        let old = Arc::new(Fixed(1));
        let new = Arc::new(Fixed(2));
        let other = Arc::new(Fixed(3));
        for (owner, id) in [(&old, "a"), (&new, "a"), (&other, "b")] {
            register(
                Entry::cache("plan_test_newest", scope(id), SizeSource::Default),
                owner,
                usage,
            );
        }
        let listed = || -> Vec<(String, u64)> {
            rows("plan_test_newest")
                .iter()
                .map(|row| {
                    let id = row.entry.scope.collection().unwrap().to_string();
                    (id, row.usage.capacity_bytes)
                })
                .collect()
        };
        assert_eq!(listed(), [("a".into(), 2), ("b".into(), 3)]);
        drop(new);
        assert_eq!(
            listed(),
            [("a".into(), 1), ("b".into(), 3)],
            "the older owner is listed again"
        );
        drop((old, other));
    }

    /// A budget's high-water mark is taken through the plan, newest owner
    /// first, and only while the budget lives.
    #[test]
    fn peaks_are_taken_from_live_budgets() {
        let _serial = serial();
        struct Marked(AtomicU64);
        fn take(marked: &Marked) -> u64 {
            marked.0.swap(0, Ordering::Relaxed)
        }
        let taken = || -> Vec<u64> {
            take_peaks()
                .into_iter()
                .filter(|(entry, _)| entry.id == "plan_test_peak")
                .map(|(_, peak)| peak)
                .collect()
        };
        let old = Arc::new(Marked(AtomicU64::new(5)));
        let new = Arc::new(Marked(AtomicU64::new(9)));
        for owner in [&old, &new] {
            register_budget(
                Entry::budget("plan_test_peak", Scope::Process, SizeSource::Default),
                owner,
                |_| Usage::budget(10, 0),
                take,
            );
        }
        assert_eq!(taken(), [9], "the newest owner");
        assert_eq!(taken(), [0], "taking restarts the mark");
        // A budget registered without a mark has none to take.
        let plain = Arc::new(Fixed(3));
        register(
            Entry::budget("plan_test_peak_none", Scope::Process, SizeSource::Default),
            &plain,
            usage,
        );
        assert!(take_peaks()
            .iter()
            .all(|(entry, _)| entry.id != "plan_test_peak_none"));
        drop(new);
        assert_eq!(taken(), [5], "the older owner again");
        drop(old);
        assert!(taken().is_empty());
    }

    #[test]
    fn rows_are_ordered_caches_first_then_by_scope() {
        let _serial = serial();
        let owner = Arc::new(Fixed(1));
        let entries = [
            Entry::budget("plan_test_order_b", Scope::Process, SizeSource::Default),
            Entry::cache(
                "plan_test_order_s",
                Scope::Source {
                    engine: "grib",
                    id: "g".into(),
                },
                SizeSource::Default,
            ),
            Entry::cache(
                "plan_test_order_c",
                Scope::Collection {
                    engine: "geotiff",
                    id: "c".into(),
                },
                SizeSource::Default,
            ),
            Entry::cache(
                "plan_test_order_e",
                Scope::Engine("netcdf"),
                SizeSource::Default,
            ),
            Entry::cache("plan_test_order_p", Scope::Process, SizeSource::Default),
        ];
        for entry in entries {
            register(entry, &owner, usage);
        }
        let ids: Vec<&str> = rows("plan_test_order")
            .iter()
            .map(|row| row.entry.id)
            .collect();
        assert_eq!(
            ids,
            [
                "plan_test_order_p",
                "plan_test_order_e",
                "plan_test_order_c",
                "plan_test_order_s",
                "plan_test_order_b",
            ]
        );
    }

    #[test]
    fn sources_print_in_the_plan_vocabulary() {
        let printed: Vec<String> = [
            SizeSource::Default,
            SizeSource::Env("MC_PVOL_PIXEL_CACHE_MB"),
            SizeSource::Config("[server] metatile_cache_mb"),
            SizeSource::Collection("[grib] grid_cache_mb"),
            SizeSource::Derived("[grib] message_cache_mb"),
            SizeSource::Constant,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            printed,
            [
                "default",
                "env:MC_PVOL_PIXEL_CACHE_MB",
                "config:[server] metatile_cache_mb",
                "collection:[grib] grid_cache_mb",
                "derived:[grib] message_cache_mb",
                "constant",
            ]
        );
        let key = SizeSource::Collection("[grib] grid_cache_mb");
        assert_eq!(
            SizeSource::unless_default(256, 256, key.clone()),
            SizeSource::Default
        );
        assert_eq!(SizeSource::unless_default(512, 256, key.clone()), key);
    }

    /// An hour of a cache of 1000 bytes: `hits`, `misses`, bytes admitted
    /// and ghost hits.
    fn hour(hits: u64, misses: u64, inserted_bytes: u64, ghost_hits: Option<[u64; 3]>) -> Activity {
        Activity {
            secs: 3600,
            capacity_bytes: 1000,
            hits,
            misses,
            churn: Churn {
                inserted_bytes,
                ghost_hits,
                ..Churn::default()
            },
        }
    }

    /// #1308 acceptance: a cache with no lookups in the hour is idle,
    /// whatever its fill and turnover; so is one under 60 lookups an hour.
    #[test]
    fn a_cache_without_lookups_is_idle_whatever_its_fill() {
        assert_eq!(hour(0, 0, 50_000, Some([0; 3])).label(), Some(Label::Idle));
        assert_eq!(hour(0, 0, 0, None).label(), Some(Label::Idle));
        assert_eq!(hour(30, 29, 50_000, None).label(), Some(Label::Idle));
        // 30 lookups in 20 minutes are 90 an hour.
        let short = Activity {
            secs: 1200,
            ..hour(0, 30, 0, None)
        };
        assert_eq!(short.label(), Some(Label::Healthy));
    }

    #[test]
    fn labels_follow_turnover_and_ghost_hits() {
        // One capacity an hour or less: healthy, whatever it misses.
        assert_eq!(
            hour(10, 90, 1000, Some([0; 3])).label(),
            Some(Label::Healthy)
        );
        // Churning: thrashing when 20 % of misses come back within 1 ×,
        assert_eq!(
            hour(10, 90, 5000, Some([0, 18, 30])).label(),
            Some(Label::Thrashing)
        );
        // a scan when under 5 % come back within 2 ×,
        assert_eq!(
            hour(10, 90, 5000, Some([0, 0, 4])).label(),
            Some(Label::Scan)
        );
        // healthy between,
        assert_eq!(
            hour(10, 90, 5000, Some([0, 10, 10])).label(),
            Some(Label::Healthy)
        );
        // and churning when no ring can tell.
        assert_eq!(hour(10, 90, 5000, None).label(), Some(Label::Churning));
        // No misses, nothing to recover.
        assert_eq!(
            hour(100, 0, 5000, Some([0; 3])).label(),
            Some(Label::Healthy)
        );
        // A disabled cache with demand has no label.
        let disabled = Activity {
            capacity_bytes: 0,
            ..hour(0, 100, 0, None)
        };
        assert_eq!(disabled.label(), None);
    }

    #[test]
    fn ghost_hits_estimate_the_hit_ratio_of_a_larger_cache() {
        let window = hour(60, 40, 2000, Some([10, 20, 30]));
        assert_eq!(window.hit_ratio(), Some(0.6));
        assert_eq!(window.turnover_per_hour(), Some(2.0));
        assert_eq!(window.ghost_share(), Some([0.25, 0.5, 0.75]));
        assert_eq!(window.estimated_hit_ratios(), Some([0.7, 0.8, 0.9]));
        assert_eq!(hour(60, 40, 2000, None).estimated_hit_ratios(), None);
        assert_eq!(hour(0, 0, 0, Some([0; 3])).estimated_hit_ratios(), None);
    }

    /// Samples at most every five minutes; the window reaches back to the
    /// newest sample at least an hour old, or the oldest while there is
    /// none; a backward step starts over; a budget has no window.
    #[test]
    fn a_cache_is_sampled_into_a_window_of_the_last_hour() {
        let usage = |hits: u64| Usage {
            capacity_bytes: 100,
            hits: Some(hits),
            misses: Some(0),
            churn: Some(Churn::default()),
            ..Usage::default()
        };
        let start = Instant::now();
        let at = |minutes: u64| start + Duration::from_secs(60 * minutes);
        let mut history = History::default();
        assert_eq!(history.observe(at(0), &usage(0)), None);
        assert_eq!(history.observe(at(4), &usage(4)), None, "under 5 minutes");
        let window = history.observe(at(30), &usage(30)).unwrap();
        assert_eq!((window.secs, window.hits), (1800, 30));
        history.observe(at(35), &usage(35));
        let window = history.observe(at(61), &usage(61)).unwrap();
        assert_eq!(
            (window.secs, window.hits),
            (61 * 60, 61),
            "no sample is an hour old yet"
        );
        let window = history.observe(at(95), &usage(95)).unwrap();
        assert_eq!(
            (window.secs, window.hits),
            (3600, 60),
            "from the sample at 35"
        );
        assert_eq!(history.0.len(), 3);
        assert_eq!(
            history.observe(at(100), &usage(1)),
            None,
            "rebuilt: starts over"
        );
        assert_eq!(history.0.len(), 1);
        let mut budget = History::default();
        let reserved = Usage::budget(100, 10);
        assert_eq!(budget.observe(at(0), &reserved), None);
        assert_eq!(budget.observe(at(61), &reserved), None);
        assert!(budget.0.is_empty());
    }

    struct Hits(AtomicU64);

    fn hits(owner: &Hits) -> Usage {
        Usage {
            capacity_bytes: 100,
            hits: Some(owner.0.load(Ordering::Relaxed)),
            misses: Some(0),
            churn: Some(Churn::default()),
            ..Usage::default()
        }
    }

    /// A snapshot samples each cache, and a row carries its window; a cache
    /// registered again keeps its history.
    #[test]
    fn snapshots_sample_each_cache_and_registering_again_keeps_its_history() {
        let _serial = serial();
        let owner = Arc::new(Hits(AtomicU64::new(0)));
        let entry = || Entry::cache("plan_test_window", Scope::Process, SizeSource::Default);
        register(entry(), &owner, hits);
        let window = |now: Instant| {
            snapshot_at(now)
                .into_iter()
                .find(|row| row.entry.id == "plan_test_window")
                .unwrap()
                .window
        };
        let start = Instant::now();
        assert_eq!(window(start), None);
        owner.0.store(120, Ordering::Relaxed);
        register(entry(), &owner, hits);
        let later = window(start + WINDOW).expect("sampled an hour ago");
        assert_eq!((later.secs, later.hits), (3600, 120));
        assert_eq!(later.label(), Some(Label::Healthy));
    }

    #[test]
    fn env_size_says_whether_the_variable_set_it() {
        assert_eq!(
            env_size("DS_CACHE_PLAN_TEST_UNSET", 42),
            (42, SizeSource::Default)
        );
        std::env::set_var("DS_CACHE_PLAN_TEST_SET", " 2048 ");
        assert_eq!(
            env_size("DS_CACHE_PLAN_TEST_SET", 42),
            (2048, SizeSource::Env("DS_CACHE_PLAN_TEST_SET"))
        );
        std::env::set_var("DS_CACHE_PLAN_TEST_ZERO", "0");
        assert_eq!(
            env_bytes("DS_CACHE_PLAN_TEST_ZERO", 42),
            (0, SizeSource::Env("DS_CACHE_PLAN_TEST_ZERO"))
        );
        // Unparseable: the default, as `env_mb` falls back.
        std::env::set_var("DS_CACHE_PLAN_TEST_BAD", "lots");
        assert_eq!(
            env_bytes("DS_CACHE_PLAN_TEST_BAD", 1),
            (MIB, SizeSource::Default)
        );
    }
}
