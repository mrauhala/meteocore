//! What each cache must hold for the data its collections serve (#1309).
//!
//! An engine knows what its caches need from facts it already holds: the
//! scans in a satellite collection's time window, a PVOL source's sites and
//! volumes, a GRIB source's grid. It computes a [`CacheDemand`] where those
//! facts change, in its poll or catalog refresh and never per request, and
//! publishes it through its [`DemandSlot`]. The plan sums the slots of one
//! cache into its row's [`Demand`] (a process-wide cache has a slot per
//! collection), and suggests a size from it ([`Demand::suggested_bytes`]).
//!
//! A cache whose configured size is below its demand is a [`Shortfall`],
//! reported once, with hysteresis so that a demand that grows a little each
//! poll, or hovers around the size, is not reported again and again. After
//! a report the cache is reported again only when its size changes, or when
//! its demand grows past the reported one by [`HEADROOM_PERCENT`]; it is
//! forgotten only once its demand drops under [`FORGET_PERCENT`] of the
//! size. A publish returns the shortfall for its engine to log, and so does
//! [`check_rows`] over a snapshot, which sees a size a reload changed. A
//! disabled cache (size 0) is never short: 0 is the way to turn one off.
//!
//! Nothing here changes a size.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};

use super::{Entry, Kind, Row, Scope, NEXT_SEQ, REGISTRY};
use crate::MIB;

/// The fields a cache of decoded or compressed fields is sized to hold:
/// the GRIB grid and message caches, the NetCDF field cache and the
/// Icechunk decoded-chunk cache. Sixteen is a short animation loop, or the
/// fields one GRIB EDR query reads at once on a 0.25° grid.
pub const DEMAND_FIELDS: u64 = 16;

/// The headroom a suggested size adds to a demand, in percent. A floor gets
/// none: what lies above a floor is the requests' working set, which no
/// engine can count. Also how far a reported demand must grow to be
/// reported again.
pub const HEADROOM_PERCENT: u64 = 15;

/// Below this share of its size, in percent, a cache reported short is
/// forgotten: a demand hovering around the size is reported once.
pub const FORGET_PERCENT: u64 = 90;

/// One collection's demand on one cache: what the cache must hold for its
/// data, computed by its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheDemand {
    pub bytes: u64,
    /// How `bytes` is computed, for the whole cache: the same for every
    /// collection of a process-wide cache.
    pub rule: &'static str,
    /// A floor rather than the demand: the requests' own working set comes
    /// on top of it.
    pub floor: bool,
    /// The engine fills the cache ahead of requests (a poll's downloads, a
    /// pre-warm, grids computed ahead), rather than requests filling it as
    /// they read: what a shortfall costs.
    pub ahead: bool,
    /// The setting that sizes the cache: an environment variable or a
    /// config key.
    pub setting: &'static str,
    /// The facts `rule` reads, by name, in order. A name ending in `_bytes`
    /// is a size.
    pub inputs: Vec<(&'static str, u64)>,
}

impl CacheDemand {
    /// A demand of `bytes`, from `rule`, for the cache `setting` sizes.
    pub fn demand(bytes: u64, rule: &'static str, setting: &'static str) -> Self {
        CacheDemand {
            bytes,
            rule,
            floor: false,
            ahead: false,
            setting,
            inputs: Vec::new(),
        }
    }

    /// A floor of `bytes`: [`Self::demand`], below which the cache cannot
    /// hold even what its engine puts there ahead of the requests.
    pub fn floor(bytes: u64, rule: &'static str, setting: &'static str) -> Self {
        CacheDemand {
            floor: true,
            ahead: true,
            ..Self::demand(bytes, rule, setting)
        }
    }

    /// For a cache the engine fills ahead of requests.
    pub fn ahead(mut self) -> Self {
        self.ahead = true;
        self
    }

    /// With one more input.
    pub fn input(mut self, name: &'static str, value: u64) -> Self {
        self.inputs.push((name, value));
        self
    }

    /// The size this demand alone suggests ([`Demand::suggested_bytes`]).
    pub fn suggested_bytes(&self) -> u64 {
        suggested(self.bytes, self.floor)
    }
}

/// The size to configure for a demand of `bytes`: plus [`HEADROOM_PERCENT`],
/// or a `floor` itself, rounded up to whole MiB, the unit of every cache
/// setting.
fn suggested(bytes: u64, floor: bool) -> u64 {
    let headroom = if floor {
        0
    } else {
        bytes.saturating_mul(HEADROOM_PERCENT).div_ceil(100)
    };
    bytes
        .saturating_add(headroom)
        .div_ceil(MIB)
        .saturating_mul(MIB)
}

/// A cache's demand: its collections' [`CacheDemand`]s summed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demand {
    pub bytes: u64,
    pub rule: &'static str,
    pub floor: bool,
    pub ahead: bool,
    pub setting: &'static str,
    /// Each collection's demand, by collection id.
    pub parts: Vec<(String, CacheDemand)>,
}

impl Demand {
    /// The sum of `parts`, sorted by collection; `None` without any.
    fn of(mut parts: Vec<(String, CacheDemand)>) -> Option<Self> {
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        let first = &parts.first()?.1;
        Some(Demand {
            bytes: parts
                .iter()
                .fold(0u64, |sum, (_, part)| sum.saturating_add(part.bytes)),
            rule: first.rule,
            floor: first.floor,
            ahead: first.ahead,
            setting: first.setting,
            parts,
        })
    }

    /// The size to configure: the demand plus [`HEADROOM_PERCENT`], or the
    /// floor itself, rounded up to whole MiB, the unit of every cache
    /// setting.
    pub fn suggested_bytes(&self) -> u64 {
        suggested(self.bytes, self.floor)
    }

    /// `demand` or `floor`.
    pub fn kind(&self) -> &'static str {
        if self.floor {
            "floor"
        } else {
            "demand"
        }
    }

    /// Whether a cache of `capacity_bytes` is too small for it. A disabled
    /// cache (0) never is.
    pub fn exceeds(&self, capacity_bytes: u64) -> bool {
        capacity_bytes > 0 && self.bytes > capacity_bytes
    }
}

struct Slot {
    seq: u64,
    cache: &'static str,
    scope: Scope,
    collection: String,
    demand: Mutex<Option<CacheDemand>>,
}

impl Slot {
    fn demand(&self) -> Option<CacheDemand> {
        self.demand
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

static SLOTS: Mutex<Vec<Weak<Slot>>> = Mutex::new(Vec::new());

/// Where an engine publishes one collection's demand on one cache. Owned
/// by the engine: its demand leaves the plan when the engine is dropped.
/// Of two live slots for the same cache, scope and collection (a reload's
/// new engine, and the old one a request still holds), the newer counts.
pub struct DemandSlot(Arc<Slot>);

impl DemandSlot {
    /// A slot for `collection`'s demand on the cache the plan lists as
    /// `cache` in `scope`, empty until the first [`Self::publish`].
    pub fn new(cache: &'static str, scope: Scope, collection: impl Into<String>) -> Self {
        let slot = Arc::new(Slot {
            // The plan's own sequence: the entries registered after this
            // slot are its engine's ([`check`]).
            seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
            cache,
            scope,
            collection: collection.into(),
            demand: Mutex::new(None),
        });
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|slot| slot.strong_count() > 0);
        slots.push(Arc::downgrade(&slot));
        DemandSlot(slot)
    }

    /// Publish `demand`, and check the cache against its demand: the
    /// [`Shortfall`] to log when it has just fallen short. From the
    /// engine's poll or refresh, never per request: it reads the cache's
    /// usage and every slot.
    pub fn publish(&self, demand: CacheDemand) -> Option<Shortfall> {
        *self.0.demand.lock().unwrap_or_else(|e| e.into_inner()) = Some(demand);
        check(&self.0)
    }

    /// Withdraw the demand: the engine no longer knows it.
    pub fn clear(&self) {
        *self.0.demand.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// What was last published.
    pub fn get(&self) -> Option<CacheDemand> {
        self.0.demand()
    }
}

impl fmt::Debug for DemandSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DemandSlot")
            .field("cache", &self.0.cache)
            .field("scope", &self.0.scope)
            .field("collection", &self.0.collection)
            .field("demand", &self.0.demand())
            .finish()
    }
}

type Key = (&'static str, Scope);

/// Every cache's demand from the live slots that have published, by cache
/// id and scope.
pub(super) fn demands() -> BTreeMap<Key, Demand> {
    let slots: Vec<Arc<Slot>> = {
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|slot| slot.strong_count() > 0);
        slots.iter().filter_map(Weak::upgrade).collect()
    };
    // The newest slot of each cache, scope and collection.
    let mut newest: BTreeMap<(Key, &str), &Slot> = BTreeMap::new();
    for slot in &slots {
        let key = ((slot.cache, slot.scope.clone()), slot.collection.as_str());
        if newest.get(&key).is_none_or(|kept| kept.seq < slot.seq) {
            newest.insert(key, slot);
        }
    }
    let mut parts: BTreeMap<Key, Vec<(String, CacheDemand)>> = BTreeMap::new();
    for ((key, collection), slot) in newest {
        if let Some(demand) = slot.demand() {
            parts
                .entry(key)
                .or_default()
                .push((collection.to_string(), demand));
        }
    }
    parts
        .into_iter()
        .filter_map(|(key, parts)| Some((key, Demand::of(parts)?)))
        .collect()
}

/// A cache configured below its demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortfall {
    pub entry: Entry,
    pub capacity_bytes: u64,
    pub demand: Demand,
}

/// `bytes` in the largest unit that keeps it above 1: B, KiB or MiB.
fn size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    }
}

impl fmt::Display for Shortfall {
    /// One line: the cache, its size and where that came from, its demand
    /// and the rule, each collection's part with the inputs it was
    /// computed from, and the size to set.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Shortfall {
            entry,
            capacity_bytes,
            demand,
        } = self;
        f.write_str(entry.id)?;
        if let Some(collection) = entry.scope.collection() {
            write!(f, " {collection}")?;
        }
        write!(
            f,
            " is {} ({}), below its {} of {} ({}): ",
            size(*capacity_bytes),
            entry.source,
            demand.kind(),
            size(demand.bytes),
            demand.rule,
        )?;
        for (i, (collection, part)) in demand.parts.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{collection} {}", size(part.bytes))?;
            for (j, (name, value)) in part.inputs.iter().enumerate() {
                f.write_str(if j == 0 { " from " } else { ", " })?;
                if name.ends_with("_bytes") {
                    write!(f, "{name} {}", size(*value))?;
                } else {
                    write!(f, "{name} {value}")?;
                }
            }
        }
        f.write_str(if demand.ahead {
            ". Below it, what the engines put there ahead of requests is evicted before a \
             request reads it"
        } else {
            ". Below it, what requests read there is evicted before they read it again"
        })?;
        write!(
            f,
            ": set {} to {} or more",
            demand.setting,
            size(demand.suggested_bytes()),
        )
    }
}

/// A shortfall reported: the size and the demand it was reported at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reported {
    capacity_bytes: u64,
    demand_bytes: u64,
}

static REPORTED: Mutex<BTreeMap<Key, Reported>> = Mutex::new(BTreeMap::new());

/// `bytes` × `percent` / 100.
fn percent(bytes: u64, percent: u64) -> u64 {
    bytes.saturating_mul(percent) / 100
}

/// Note `entry` at `capacity_bytes` against `demand`: the shortfall to
/// report when the cache is short and has not been reported; after a
/// report, only when its size changed, or its demand grew past the
/// reported one by [`HEADROOM_PERCENT`]. A report is forgotten when the
/// cache is disabled, has no demand, or its demand drops under
/// [`FORGET_PERCENT`] of its size.
fn note(entry: &Entry, capacity_bytes: u64, demand: Option<&Demand>) -> Option<Shortfall> {
    let key = (entry.id, entry.scope.clone());
    let mut reported = REPORTED.lock().unwrap_or_else(|e| e.into_inner());
    let forget = demand.is_none_or(|demand| {
        capacity_bytes == 0 || demand.bytes < percent(capacity_bytes, FORGET_PERCENT)
    });
    if forget {
        reported.remove(&key);
        return None;
    }
    let demand = demand.filter(|demand| demand.exceeds(capacity_bytes))?;
    let again = reported.get(&key).is_none_or(|last| {
        last.capacity_bytes != capacity_bytes
            || demand.bytes
                > last
                    .demand_bytes
                    .saturating_add(percent(last.demand_bytes, HEADROOM_PERCENT))
    });
    if !again {
        return None;
    }
    reported.insert(
        key,
        Reported {
            capacity_bytes,
            demand_bytes: demand.bytes,
        },
    );
    Some(Shortfall {
        entry: entry.clone(),
        capacity_bytes,
        demand: demand.clone(),
    })
}

/// Check every cache of `rows`, the whole plan as [`super::snapshot`]
/// lists it, against its demand: the shortfalls to report (see the module
/// docs). A cache `rows` no longer lists is forgotten, so a collection
/// removed and added again is reported afresh.
pub fn check_rows(rows: &[Row]) -> Vec<Shortfall> {
    let caches = || rows.iter().filter(|row| row.entry.kind == Kind::Cache);
    let shortfalls = caches()
        .filter_map(|row| note(&row.entry, row.usage.capacity_bytes, row.demand.as_ref()))
        .collect();
    let listed: std::collections::BTreeSet<Key> = caches()
        .map(|row| (row.entry.id, row.entry.scope.clone()))
        .collect();
    REPORTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|key, _| listed.contains(key));
    shortfalls
}

/// Check `slot`'s cache against its demand, if the plan lists it.
///
/// A collection's or source's cache is entered in the plan for each engine,
/// after the engine is built, so only an entry registered after the slot is
/// its engine's: until then, during a reload, the plan lists the replaced
/// engine's cache, whose size may be the old one, or, for a rejected
/// reload's candidate, nothing of its own. Its publishes check nothing
/// until its engine is entered; the next snapshot's [`check_rows`] does.
/// A process-wide cache is one, whatever engine publishes.
fn check(slot: &Slot) -> Option<Shortfall> {
    let per_engine = matches!(slot.scope, Scope::Collection { .. } | Scope::Source { .. });
    let registered = {
        let registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        registry
            .iter()
            .filter(|r| {
                r.owner.strong_count() > 0
                    && r.entry.kind == Kind::Cache
                    && r.entry.id == slot.cache
                    && r.entry.scope == slot.scope
                    && (!per_engine || r.seq > slot.seq)
            })
            .max_by_key(|r| r.seq)
            .cloned()?
    };
    // Read outside the lock, as `snapshot` does.
    let usage = (registered.read)()?;
    let demand = demands().remove(&(slot.cache, slot.scope.clone()));
    note(&registered.entry, usage.capacity_bytes, demand.as_ref())
}

#[cfg(test)]
mod tests {
    use super::super::{register, serial, snapshot, SizeSource, Usage};
    use super::*;

    struct Sized(u64);

    fn usage(sized: &Sized) -> Usage {
        Usage {
            capacity_bytes: sized.0,
            ..Usage::default()
        }
    }

    fn scope(id: &str) -> Scope {
        Scope::Collection {
            engine: "geotiff",
            id: id.into(),
        }
    }

    fn demand(bytes: u64) -> CacheDemand {
        CacheDemand::demand(bytes, "frames × frame bytes", "[geotiff] tile_cache_mb")
            .input("frames", 4)
            .input("frame_bytes", bytes / 4)
    }

    /// The rows of this test's own cache id: tests share the registry.
    fn row(id: &str) -> Row {
        snapshot()
            .into_iter()
            .find(|row| row.entry.id == id)
            .unwrap()
    }

    #[test]
    fn suggestions_add_headroom_to_a_demand_and_none_to_a_floor() {
        let of = |bytes, floor| Demand {
            bytes,
            rule: "",
            floor,
            ahead: floor,
            setting: "",
            parts: Vec::new(),
        };
        assert_eq!(of(100 * MIB, false).suggested_bytes(), 115 * MIB);
        // Rounded up to whole MiB.
        assert_eq!(of(MIB + 1, false).suggested_bytes(), 2 * MIB);
        assert_eq!(of(10 * MIB + 1, true).suggested_bytes(), 11 * MIB);
        assert_eq!(of(0, false).suggested_bytes(), 0);
        assert!(of(2, false).exceeds(1));
        assert!(!of(2, false).exceeds(2));
        assert!(!of(2, false).exceeds(0), "a disabled cache is never short");
    }

    /// A process-wide cache sums its collections' slots, the newest of each
    /// collection, and a dropped slot leaves it.
    #[test]
    fn a_cache_sums_the_newest_slot_of_each_collection() {
        let _serial = serial();
        let owner = Arc::new(Sized(100));
        let scope = Scope::Engine("demand-test-sum");
        register(
            Entry::cache("demand_test_sum", scope.clone(), SizeSource::Default),
            &owner,
            usage,
        );
        let slot = |collection: &str, bytes| {
            let slot = DemandSlot::new("demand_test_sum", scope.clone(), collection);
            slot.publish(demand(bytes));
            slot
        };
        let a = slot("a", 10);
        let b_old = slot("b", 20);
        let b_new = slot("b", 30);
        let unpublished = DemandSlot::new("demand_test_sum", scope.clone(), "c");
        let demand = row("demand_test_sum").demand.unwrap();
        assert_eq!(demand.bytes, 40);
        let parts: Vec<(&str, u64)> = demand
            .parts
            .iter()
            .map(|(c, part)| (c.as_str(), part.bytes))
            .collect();
        assert_eq!(parts, [("a", 10), ("b", 30)]);
        drop(b_new);
        assert_eq!(
            row("demand_test_sum").demand.unwrap().bytes,
            30,
            "b's older slot"
        );
        drop((a, b_old, unpublished));
        assert_eq!(row("demand_test_sum").demand, None);
    }

    /// Note one cache of `capacity` MiB against a demand of `demand` MiB,
    /// a floor when `floor`: whether it is reported.
    fn noted(id: &'static str, floor: bool) -> impl Fn(u64, u64) -> bool {
        let entry = Entry::cache(id, scope(id), SizeSource::Default);
        move |capacity, demand| {
            let part = if floor {
                CacheDemand::floor(demand * MIB, "", "")
            } else {
                CacheDemand::demand(demand * MIB, "", "")
            };
            let demand = Demand::of(vec![(id.to_string(), part)]);
            note(&entry, capacity * MIB, demand.as_ref()).is_some()
        }
    }

    /// A shortfall is reported once; again when the size changes, or the
    /// demand grows 15 % past the one reported; not while it stays within
    /// that or hovers around the size; afresh once the demand has dropped
    /// under 90 % of the size, or the cache was disabled. Never for a
    /// disabled cache, or without a demand.
    #[test]
    fn a_shortfall_is_reported_once_per_change() {
        let _serial = serial();
        let note = noted("demand_test_once", false);
        assert!(!note(100, 50), "fits");
        assert!(note(100, 150), "falls short");
        assert!(!note(100, 150), "still short, as reported");
        assert!(!note(100, 172), "within 15 % of the reported 150");
        assert!(note(100, 173), "grew 15 % past it");
        assert!(note(120, 173), "resized, still short");
        assert!(!note(120, 110), "fits, but within 10 %: remembered");
        assert!(!note(120, 130), "short again, as reported");
        assert!(!note(120, 100), "under 90 %: forgotten");
        assert!(note(120, 130), "short afresh");
        assert!(!note(0, 180), "disabled");
        assert!(note(120, 180), "enabled again, short");
        assert!(!note(120, 0), "no demand");
        let entry = Entry::cache(
            "demand_test_once",
            scope("demand_test_once"),
            SizeSource::Default,
        );
        assert_eq!(super::note(&entry, 120 * MIB, None), None);
        assert!(note(120, 180), "a demand again");
    }

    /// A PVOL floor growing by a 2.9 MB sweep each poll, or hovering either
    /// side of the size, is reported once over 20 polls (the reviewed
    /// repeat: 20 and 9 reports before).
    #[test]
    fn a_growing_or_hovering_floor_is_reported_once() {
        let _serial = serial();
        let growing = noted("demand_test_growing", true);
        let reports = (0..20u64)
            .filter(|poll| growing(4096, 4100 + poll * 3))
            .count();
        assert_eq!(reports, 1, "growing");
        let hovering = noted("demand_test_hovering", true);
        let reports = (0..20u64)
            .filter(|poll| hovering(4096, if poll % 2 == 0 { 4110 } else { 4090 }))
            .count();
        assert_eq!(reports, 1, "hovering");
        // Resized, still short: reported; well below, then back: reported.
        assert!(hovering(4100, 4110));
        assert!(!hovering(4100, 3000));
        assert!(hovering(4100, 4110));
    }

    /// A publish checks its cache: the first one short is reported, with
    /// the cache, its size and source, the rule, each part's inputs and the
    /// size to set, and what a request-filled cache's shortfall costs; a
    /// snapshot's check then reports nothing new.
    #[test]
    fn a_publish_that_falls_short_reports_the_inputs_once() {
        let _serial = serial();
        let id = "demand_test_publish";
        // The engine is built, with its slot, before the plan enters its
        // cache.
        let slot = DemandSlot::new(id, scope("opera"), "opera");
        let owner = Arc::new(Sized(8 * MIB));
        register(
            Entry::cache(
                id,
                scope("opera"),
                SizeSource::Collection("[geotiff] tile_cache_mb"),
            ),
            &owner,
            usage,
        );
        assert_eq!(slot.publish(demand(4 * MIB)), None, "fits");
        let short = slot.publish(demand(10 * MIB)).expect("short");
        assert_eq!(
            short.to_string(),
            "demand_test_publish opera is 8 MiB (collection:[geotiff] tile_cache_mb), below \
             its demand of 10 MiB (frames × frame bytes): opera 10 MiB from frames 4, \
             frame_bytes 2.50 MiB. Below it, what requests read there is evicted before they \
             read it again: set [geotiff] tile_cache_mb to 12 MiB or more"
        );
        assert_eq!(slot.publish(demand(10 * MIB)), None, "reported already");
        let rows: Vec<Row> = snapshot()
            .into_iter()
            .filter(|row| row.entry.id == id)
            .collect();
        assert_eq!(rows[0].demand.as_ref().unwrap().suggested_bytes(), 12 * MIB);
        assert!(check_rows(&rows).is_empty(), "reported already");
        // Gone from the plan, then back: reported afresh.
        assert!(check_rows(&[]).is_empty());
        assert_eq!(check_rows(&rows).len(), 1, "listed again");
        // A cache filled ahead of requests says what that costs.
        let ahead = Demand::of(vec![("opera".into(), demand(10 * MIB).ahead())]).unwrap();
        let text = Shortfall {
            demand: ahead,
            ..short
        }
        .to_string();
        assert!(
            text.contains(
                "Below it, what the engines put there ahead of requests is evicted before a \
                 request reads it"
            ),
            "{text}"
        );
        // A slot for a cache the plan does not list checks nothing.
        let unlisted = DemandSlot::new("demand_test_unlisted", scope("opera"), "opera");
        assert_eq!(unlisted.publish(demand(10 * MIB)), None);
        drop(owner);
    }

    /// During a reload the plan still lists the replaced engine's cache of
    /// a collection: a new engine's publish checks nothing until its own
    /// cache is entered, after its slot. A process-wide cache entered
    /// before a slot is that slot's cache all the same.
    #[test]
    fn a_publish_checks_only_its_own_engine_s_cache() {
        let _serial = serial();
        let id = "demand_test_reload";
        let old = Arc::new(Sized(MIB));
        register(
            Entry::cache(id, scope("grib"), SizeSource::Default),
            &old,
            usage,
        );
        let slot = DemandSlot::new(id, scope("grib"), "grib");
        assert_eq!(
            slot.publish(demand(4 * MIB)),
            None,
            "the old engine's 1 MiB"
        );
        let new = Arc::new(Sized(2 * MIB));
        register(
            Entry::cache(id, scope("grib"), SizeSource::Default),
            &new,
            usage,
        );
        let short = slot.publish(demand(4 * MIB)).expect("its own 2 MiB");
        assert_eq!(short.capacity_bytes, 2 * MIB);

        let process = Scope::Engine("demand-test-process");
        let cache = Arc::new(Sized(MIB));
        register(
            Entry::cache(id, process.clone(), SizeSource::Default),
            &cache,
            usage,
        );
        let later = DemandSlot::new(id, process, "late");
        assert!(later.publish(demand(4 * MIB)).is_some());
        drop((old, new, cache));
    }

    #[test]
    fn sizes_print_in_the_largest_unit_above_one() {
        assert_eq!(size(180), "180 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(3 * MIB), "3 MiB");
        assert_eq!(size(MIB + MIB / 4), "1.25 MiB");
    }
}
