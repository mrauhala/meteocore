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

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::{CacheMetrics, MIB};

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
/// `config:[server] …`, `collection:[grib] …` or `constant`.
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

/// What one cache or budget holds now. Hits and misses are cumulative since
/// it was built; a budget has no entries, hits or misses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// The configured size; `0` for a disabled cache.
    pub capacity_bytes: u64,
    /// Resident bytes of a cache, reserved bytes of a budget.
    pub used_bytes: u64,
    pub entries: Option<u64>,
    pub hits: Option<u64>,
    pub misses: Option<u64>,
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
    let registered = Arc::new(Registered {
        seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
        owner_addr,
        owner: any,
        read: Box::new(move || weak.upgrade().map(|owner| usage(&owner))),
        take_peak,
        entry,
    });
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    registry.retain(|r| {
        r.owner.strong_count() > 0
            && !(r.owner_addr == owner_addr && r.entry.id == registered.entry.id)
    });
    registry.push(registered);
}

/// Every live entry with its usage, caches before budgets, then by scope
/// (process, engine, collection, source) and id. Where two live entries
/// share an id and scope (an engine replaced by a reload, still held by a
/// request that started before it), the one registered last is listed.
pub fn snapshot() -> Vec<Row> {
    let registered: Vec<Arc<Registered>> = {
        let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        registry.retain(|r| r.owner.strong_count() > 0);
        registry.clone()
    };
    // Read outside the lock: a cache's usage takes its own shard locks.
    let mut rows: Vec<(u64, Row)> = registered
        .iter()
        .filter_map(|r| {
            let usage = (r.read)()?;
            Some((
                r.seq,
                Row {
                    entry: r.entry.clone(),
                    usage,
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
