//! The memory plan's sizes (#1312): what each cache and admission budget
//! would be, fitted to the memory the process may use.
//!
//! [`plan`] reads [`Inputs`]: the total, how the memory outside the caches
//! was found, and every cache and budget of a [`super::snapshot`], with its
//! demand and whether a collection can reach it. It returns a [`Plan`]: a
//! [`Grant`] per cache and budget, each with its [`Reason`], and the
//! reserves and pool they were fitted to. It is pure and deterministic, the
//! same inputs in any order giving the same plan, and applies nothing.
//!
//! Reserves come first: the memory outside jemalloc, measured but at least
//! [`OUTSIDE_FLOOR`]; the heap of the stores plus [`HEAP_PER_COLLECTION`]
//! for what has no gauge (unmeasured, [`ESTIMATED_BASELINE`] and
//! [`ESTIMATED_PER_COLLECTION`] stand for both); the render budget and the
//! largest read budget, scaled to the total ([`RENDER_BUDGET`],
//! [`READ_BUDGET`], a read budget never above its size today). What they leave, less [`SLACK_PERCENT`] of allocator
//! slack, is the pool the caches share:
//!
//! 1. A pinned cache, sized by a setting, keeps its size.
//! 2. A cache no collection can reach gets 0.
//! 3. A demand cache gets its demand plus 15 %.
//! 4. A working-set cache gets its estimate (its demand plus 15 %, else its
//!    size today) between [`WORKING_SET_MIN`] and [`WORKING_SET_MAX`], all
//!    of them together at most [`WORKING_SET_PERCENT`] of what the first
//!    three leave.
//! 5. The traffic-driven caches share what is left by weight ([`class`]),
//!    each between its floor and its ceiling ([`TRAFFIC_RANGE`]).
//! 6. While the total allows, no cache gets less than its size today.
//! 7. When it does not, caches are cut toward their floors in [`Step`]
//!    order until they fit. What still does not fit is the plan's
//!    shortfall, never an error.
//! 8. A grant carries the cache's entry size, for its shard layout.
//!
//! A cache's expected use is what the plan counts it at: its grant for a
//! traffic-driven cache, a pinned or fixed one, and one whose demand is not
//! known; else its demand, or its used bytes when larger, up to its grant;
//! 0 when no collection can reach it. The plan fits the expected use into
//! the pool, so the grants may sum to more, as configured sizes do today:
//! a cache allocates only as it fills.

use std::fmt;

use super::{Demand, Entry, Kind, Label, Row, SizeSource};
use crate::{FLOOR_ENTRIES, MIB};

/// One gibibyte.
pub const GIB: u64 = 1024 * MIB;

/// Below this total, every cache and budget the plan sizes is at its floor.
pub const MIN_TOTAL: u64 = 512 * MIB;

/// The least the plan reserves for memory outside jemalloc (thread stacks,
/// mapped files), whatever it measured.
pub const OUTSIDE_FLOOR: u64 = 512 * MIB;

/// Heap reserved per collection beside the store gauges, for what no gauge
/// meters.
pub const HEAP_PER_COLLECTION: u64 = 32 * MIB;

/// The memory outside the caches of a process not measured: this, plus
/// [`ESTIMATED_PER_COLLECTION`] per collection.
pub const ESTIMATED_BASELINE: u64 = 256 * MIB;

/// See [`ESTIMATED_BASELINE`].
pub const ESTIMATED_PER_COLLECTION: u64 = 16 * MIB;

/// Allocator slack on what the caches get, in percent: the pool is what
/// the other reserves leave, divided by 1.1.
pub const SLACK_PERCENT: u64 = 10;

/// A budget scaled to the total: `percent` of it, between `min` and `max`.
/// `min` is also the floor a cut takes it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scaled {
    pub percent: u64,
    pub min: u64,
    pub max: u64,
}

impl Scaled {
    /// `percent` of `total_bytes` in whole MiB, between `min` and `max`.
    pub fn of(&self, total_bytes: u64) -> u64 {
        mib_down(total_bytes / 100 * self.percent).clamp(self.min, self.max)
    }
}

/// The render budget: 10 % of the total, 128 MiB to 4 GiB. 128 MiB still
/// admits a 2048 × 2048 frame at 32 bytes a pixel.
pub const RENDER_BUDGET: Scaled = Scaled {
    percent: 10,
    min: 128 * MIB,
    max: 4 * GIB,
};

/// Every other budget: 5 % of the total, 64 MiB to 1 GiB, and never above
/// its size today: what one of 512 MiB holds at its peak is not known
/// until the high-water gauges (`<budget>_reserved_peak_bytes`, #1307)
/// have production data. Only the largest is reserved: reads of different
/// engines rarely peak together.
pub const READ_BUDGET: Scaled = Scaled {
    percent: 5,
    min: 64 * MIB,
    max: GIB,
};

/// The render budget's id; every other budget is a read budget.
pub const RENDER_BUDGET_ID: &str = "render_budget";

/// The most the working-set caches get together, in percent of what the
/// pinned, fixed and demand caches leave of the pool.
pub const WORKING_SET_PERCENT: u64 = 35;

/// A working-set cache's estimate is clamped to this and
/// [`WORKING_SET_MAX`]; a cut takes it here, or to its size today when
/// that is less.
pub const WORKING_SET_MIN: u64 = 64 * MIB;

/// See [`WORKING_SET_MIN`].
pub const WORKING_SET_MAX: u64 = 2 * GIB;

/// The least floor of a traffic-driven or demand cache.
pub const MIN_FLOOR: u64 = 16 * MIB;

/// A traffic-driven cache's floor is its size today divided by this, its
/// ceiling its size today times this.
pub const TRAFFIC_RANGE: u64 = 8;

/// How the plan sizes a cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Holds what its engines put there ahead of requests: its demand plus
    /// 15 %.
    Demand,
    /// Holds the fields, chunks or tiles requests read: an estimate between
    /// [`WORKING_SET_MIN`] and [`WORKING_SET_MAX`].
    WorkingSet,
    /// Holds what traffic asks for: a share of the pool by this weight.
    Traffic(u64),
    /// No rule sizes it: it keeps its size.
    Fixed,
}

/// The class of the cache whose `/metrics` family is `id`.
pub fn class(id: &str) -> Class {
    match id {
        "metatile_cache" => Class::Traffic(45),
        "pvol_pixel_cache" => Class::Traffic(25),
        "geotiff_decoded_chunk_cache" => Class::Traffic(15),
        "odim_composite_cache" => Class::Traffic(10),
        "rendered_cache"
        | "vector_tile_cache"
        | "tiles3d_content_cache"
        | "pvol_voxel_grid_cache" => Class::Traffic(5),
        "satellite_frame_cache" | "station_grid_cache" => Class::Demand,
        "grid_cache"
        | "grib_message_cache"
        | "tile_cache"
        | "zarr_decoded_cache"
        | "zarr_object_cache"
        | "zarr_icechunk_payload_cache"
        | "netcdf_field_cache"
        | "netcdf_chunk_cache"
        | "netcdf_block_cache" => Class::WorkingSet,
        _ => Class::Fixed,
    }
}

/// Whether a size came from a setting: an environment variable, a config
/// key, or a key it follows. A pinned size is never changed.
pub fn pinned(source: &SizeSource) -> bool {
    !matches!(source, SizeSource::Default | SizeSource::Constant)
}

/// A cache as the plan reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheInput {
    pub entry: Entry,
    /// Its size today. Unpinned, the built-in default.
    pub capacity_bytes: u64,
    pub used_bytes: u64,
    pub entries: Option<u64>,
    pub demand: Option<Demand>,
    /// Its last hour's [`Label`] is [`Label::Idle`]: no demand yet.
    pub idle: bool,
    /// Why no collection can reach it, when none can.
    pub unreachable: Option<String>,
}

impl CacheInput {
    /// The size of one entry ([`Grant::entry_bytes`]): what its engines
    /// publish, else the mean of what it holds.
    pub fn entry_bytes(&self) -> Option<u64> {
        self.demand
            .as_ref()
            .and_then(Demand::entry_bytes)
            .or_else(|| {
                self.entries
                    .filter(|entries| *entries > 0)
                    .map(|entries| self.used_bytes / entries)
            })
    }
}

/// An admission budget as the plan reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetInput {
    pub entry: Entry,
    /// Its size today.
    pub capacity_bytes: u64,
    /// Why no collection can reach it, when none can.
    pub unreachable: Option<String>,
}

/// How the memory outside the caches is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Baseline {
    /// From the running process: resident memory less jemalloc's
    /// (`outside_bytes`), and the heap the store gauges report.
    Measured {
        outside_bytes: u64,
        store_bytes: u64,
    },
    /// Estimated from the number of collections.
    Estimated,
}

/// What [`plan`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub total_bytes: u64,
    pub collections: u64,
    pub baseline: Baseline,
    pub caches: Vec<CacheInput>,
    pub budgets: Vec<BudgetInput>,
}

impl Inputs {
    /// The inputs of a snapshot's `rows`, `unreachable` saying why no
    /// collection can reach an entry, when none can.
    pub fn of_rows(
        total_bytes: u64,
        collections: u64,
        baseline: Baseline,
        rows: &[Row],
        unreachable: impl Fn(&Row) -> Option<String>,
    ) -> Self {
        let mut inputs = Inputs {
            total_bytes,
            collections,
            baseline,
            caches: Vec::new(),
            budgets: Vec::new(),
        };
        for row in rows {
            match row.entry.kind {
                Kind::Cache => inputs.caches.push(CacheInput {
                    entry: row.entry.clone(),
                    capacity_bytes: row.usage.capacity_bytes,
                    used_bytes: row.usage.used_bytes,
                    entries: row.usage.entries,
                    demand: row.demand.clone(),
                    idle: row
                        .window
                        .as_ref()
                        .is_some_and(|w| w.label() == Some(Label::Idle)),
                    unreachable: unreachable(row),
                }),
                Kind::Budget => inputs.budgets.push(BudgetInput {
                    entry: row.entry.clone(),
                    capacity_bytes: row.usage.capacity_bytes,
                    unreachable: unreachable(row),
                }),
            }
        }
        inputs
    }
}

/// A step of a cut, in the order the plan takes them (rule 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Caches with no demand yet: idle over the last hour.
    Idle,
    /// The traffic-driven caches, lowest weight first.
    Traffic,
    /// The working-set caches.
    WorkingSet,
    /// The admission budgets.
    Budgets,
    /// The demand caches, last: below its demand an engine degrades.
    Demand,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Idle => "idle",
            Step::Traffic => "traffic",
            Step::WorkingSet => "working set",
            Step::Budgets => "budget",
            Step::Demand => "below its demand",
        }
    }
}

/// Why a cache or budget got its grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Sized by a setting: kept.
    Pinned,
    /// No collection can reach it: 0.
    Unreachable(String),
    /// No rule sizes it: kept.
    Fixed,
    /// Its demand plus 15 %.
    Demand,
    /// Its working-set estimate.
    WorkingSet,
    /// Its share of the pool by this weight.
    Share(u64),
    /// Its size today, which its rule would lower (rule 6).
    Today,
    /// A budget scaled to the total.
    Scaled,
    /// Cut toward its floor in this step.
    Cut(Step),
    /// The total is under [`MIN_TOTAL`].
    Floor,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Pinned => f.write_str("pinned"),
            Reason::Unreachable(why) => write!(f, "unreachable: {why}"),
            Reason::Fixed => f.write_str("fixed"),
            Reason::Demand => f.write_str("demand + 15 %"),
            Reason::WorkingSet => f.write_str("working set"),
            Reason::Share(weight) => write!(f, "share by weight {weight}"),
            Reason::Today => f.write_str("size today"),
            Reason::Scaled => f.write_str("scaled to the total"),
            Reason::Cut(step) => write!(f, "cut: {}", step.as_str()),
            Reason::Floor => f.write_str("floor: total under 512 MiB"),
        }
    }
}

/// What the plan gives one cache or budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub entry: Entry,
    /// Its size today.
    pub capacity_bytes: u64,
    /// The size the plan gives it.
    pub bytes: u64,
    /// What the plan counts it at (module docs); a budget's grant.
    pub expected_bytes: u64,
    /// The least a cut gives it; its grant when nothing cuts it.
    pub floor_bytes: u64,
    /// The size of one entry, for the shard layout of a cache built at
    /// the grant (rule 8); `None` for a budget or when unknown.
    pub entry_bytes: Option<u64>,
    pub reason: Reason,
}

/// The reserves taken before the caches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserves {
    /// Memory outside jemalloc.
    pub outside_bytes: u64,
    /// Heap outside the caches: the stores and the per-collection reserve.
    pub heap_bytes: u64,
    /// Whether `outside_bytes` and `heap_bytes` were measured or estimated.
    pub measured: bool,
    pub render_bytes: u64,
    /// The largest read budget that a collection can reach.
    pub read_bytes: u64,
    pub read_budget: Option<&'static str>,
    pub slack_bytes: u64,
}

impl Reserves {
    /// Outside jemalloc and heap: the memory outside caches and budgets.
    pub fn baseline_bytes(&self) -> u64 {
        self.outside_bytes.saturating_add(self.heap_bytes)
    }
}

/// The plan: every cache's and budget's grant, and what they were fitted
/// to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub total_bytes: u64,
    pub reserves: Reserves,
    /// What the caches share.
    pub pool_bytes: u64,
    /// The caches' expected use, summed.
    pub expected_bytes: u64,
    /// The demands of the caches a collection can reach, summed.
    pub demand_bytes: u64,
    /// The floors of the caches the plan sizes, summed.
    pub floor_bytes: u64,
    /// The pinned caches' sizes, summed.
    pub pinned_bytes: u64,
    /// How much more the total must be for the plan to fit; 0 when it fits.
    pub shortfall_bytes: u64,
    /// Whether the caches' sizes today did not fit, so the plan cut some.
    pub cut: bool,
    pub caches: Vec<Grant>,
    pub budgets: Vec<Grant>,
}

impl Plan {
    /// The grant of `entry`, cache or budget.
    pub fn grant(&self, entry: &Entry) -> Option<&Grant> {
        let grants = match entry.kind {
            Kind::Cache => &self.caches,
            Kind::Budget => &self.budgets,
        };
        grants
            .iter()
            .find(|g| g.entry.id == entry.id && g.entry.scope == entry.scope)
    }
}

/// `bytes` rounded down to whole MiB, the unit of every setting.
fn mib_down(bytes: u64) -> u64 {
    bytes / MIB * MIB
}

/// `bytes` rounded up to whole MiB.
fn mib_up(bytes: u64) -> u64 {
    bytes.div_ceil(MIB).saturating_mul(MIB)
}

/// `bytes` × `percent` / 100.
fn percent(bytes: u64, percent: u64) -> u64 {
    bytes / 100 * percent + bytes % 100 * percent / 100
}

/// Which step of a cut moves a cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// Never cut: pinned, fixed or unreachable.
    Never,
    Idle,
    Traffic,
    WorkingSet,
    Demand,
}

impl Group {
    fn step(self) -> Option<Step> {
        match self {
            Group::Never => None,
            Group::Idle => Some(Step::Idle),
            Group::Traffic => Some(Step::Traffic),
            Group::WorkingSet => Some(Step::WorkingSet),
            Group::Demand => Some(Step::Demand),
        }
    }
}

/// A cache being planned.
struct Planned {
    grant: Grant,
    group: Group,
    /// Its size today.
    today: u64,
    /// The grant its rule gives, with rule 6.
    want: u64,
    /// What it must hold, when known: its expected use is its grant up to
    /// this. `None`: its grant.
    need: Option<u64>,
    /// A traffic-driven cache's weight, and the floor the weight sits on.
    weight: u64,
    base: u64,
    ceiling: u64,
    /// Its demand, when known.
    demand_bytes: Option<u64>,
    /// The least size worth having above its floor: a working set's
    /// [`WORKING_SET_MIN`] or four of its entries.
    useful: u64,
}

impl Planned {
    fn expected(&self, bytes: u64) -> u64 {
        self.need.map_or(bytes, |need| bytes.min(need))
    }

    fn set(&mut self, bytes: u64) {
        self.grant.bytes = bytes;
        self.grant.expected_bytes = self.expected(bytes);
    }

    /// Down to its floor when it is between that and its useful size.
    fn snap(&mut self) {
        if self.grant.bytes > self.grant.floor_bytes && self.grant.bytes < self.useful {
            self.set(self.grant.floor_bytes);
        }
    }
}

fn of_cache(input: &CacheInput, floors_only: bool) -> Planned {
    let today = input.capacity_bytes;
    let demand = input.demand.as_ref();
    let need = demand.map(|d| d.bytes.max(input.used_bytes));
    let entry_bytes = input.entry_bytes();
    let class = class(input.entry.id);
    let mut planned = Planned {
        grant: Grant {
            entry: input.entry.clone(),
            capacity_bytes: today,
            bytes: today,
            expected_bytes: today,
            floor_bytes: today,
            entry_bytes,
            reason: Reason::Fixed,
        },
        group: Group::Never,
        today,
        want: today,
        need: None,
        weight: 0,
        base: 0,
        ceiling: today,
        demand_bytes: demand.map(|d| d.bytes),
        useful: 0,
    };
    if pinned(&input.entry.source) {
        planned.grant.reason = Reason::Pinned;
        planned.need = match (&input.unreachable, class) {
            (Some(_), _) => Some(input.used_bytes),
            (None, Class::Demand | Class::WorkingSet) => need,
            (None, _) => None,
        };
        planned.set(today);
        return planned;
    }
    if let Some(why) = &input.unreachable {
        planned.grant.reason = Reason::Unreachable(why.clone());
        planned.grant.floor_bytes = 0;
        planned.want = 0;
        planned.set(0);
        return planned;
    }
    let (want, floor, reason) = match class {
        Class::Fixed => {
            planned.set(today);
            return planned;
        }
        Class::Demand => {
            planned.group = Group::Demand;
            planned.need = need;
            let rule = demand.map(Demand::suggested_bytes);
            let want = rule.unwrap_or(today).max(today);
            let entries = entry_bytes.unwrap_or(0).saturating_mul(FLOOR_ENTRIES);
            let floor = mib_up(entries.max(MIN_FLOOR)).min(want);
            let reason = match rule {
                Some(rule) if rule >= today => Reason::Demand,
                _ => Reason::Today,
            };
            (want, floor, reason)
        }
        Class::WorkingSet => {
            planned.group = if input.idle {
                Group::Idle
            } else {
                Group::WorkingSet
            };
            planned.need = need;
            let estimate = demand.map_or(today, Demand::suggested_bytes);
            let want = estimate.clamp(WORKING_SET_MIN, WORKING_SET_MAX);
            // Less than this holds too few fields to be worth having: a
            // cut below it takes the cache to its floor, 0 for one that is
            // off today.
            let entries = entry_bytes.unwrap_or(0).saturating_mul(FLOOR_ENTRIES);
            planned.useful = mib_up(entries).clamp(WORKING_SET_MIN, WORKING_SET_MAX);
            // Scaled with the others and raised to `today` later.
            (want, planned.useful.min(today), Reason::WorkingSet)
        }
        Class::Traffic(weight) => {
            planned.group = if input.idle {
                Group::Idle
            } else {
                Group::Traffic
            };
            planned.weight = weight;
            // A floor demand (the PVOL pre-warm) is what the weight sits on.
            planned.base = demand
                .filter(|d| d.floor)
                .map_or(0, Demand::suggested_bytes);
            let floor = mib_up((today / TRAFFIC_RANGE).max(MIN_FLOOR)).max(planned.base);
            let want = floor.max(today);
            planned.ceiling = today.saturating_mul(TRAFFIC_RANGE).max(want);
            (want, floor, Reason::Share(weight))
        }
    };
    planned.want = want;
    planned.grant.floor_bytes = floor;
    planned.grant.reason = if floors_only { Reason::Floor } else { reason };
    planned.set(if floors_only { floor } else { want });
    planned
}

/// A budget being planned.
struct PlannedBudget {
    grant: Grant,
    render: bool,
    /// Counted in the reserve: a collection can reach it.
    reserved: bool,
    /// Moved by a cut.
    cut: bool,
    want: u64,
}

fn of_budget(input: &BudgetInput, total: u64, floors_only: bool) -> PlannedBudget {
    let render = input.entry.id == RENDER_BUDGET_ID;
    let today = input.capacity_bytes;
    let mut planned = PlannedBudget {
        grant: Grant {
            entry: input.entry.clone(),
            capacity_bytes: today,
            bytes: today,
            expected_bytes: today,
            floor_bytes: today,
            entry_bytes: None,
            reason: Reason::Pinned,
        },
        render,
        reserved: input.unreachable.is_none(),
        cut: false,
        want: today,
    };
    if pinned(&input.entry.source) {
        return planned;
    }
    if let Some(why) = &input.unreachable {
        planned.want = 0;
        planned.grant.floor_bytes = 0;
        planned.grant.reason = Reason::Unreachable(why.clone());
        planned.set(0);
        return planned;
    }
    let scale = if render { RENDER_BUDGET } else { READ_BUDGET };
    planned.cut = true;
    planned.want = if render {
        scale.of(total)
    } else {
        // Capped at its size today until the #1307 peaks justify more.
        scale.of(total).min(today)
    };
    planned.grant.floor_bytes = scale.min.min(planned.want);
    if floors_only {
        planned.grant.reason = Reason::Floor;
        planned.set(scale.min);
    } else {
        planned.grant.reason = Reason::Scaled;
        planned.set(planned.want);
    }
    planned
}

impl PlannedBudget {
    fn set(&mut self, bytes: u64) {
        self.grant.bytes = bytes;
        self.grant.expected_bytes = bytes;
    }
}

/// The render budget and the largest read budget a collection can reach,
/// with that read budget's id.
fn reserve(budgets: &[PlannedBudget]) -> (u64, u64, Option<&'static str>) {
    let reserved = || budgets.iter().filter(|b| b.reserved);
    let render = reserved()
        .filter(|b| b.render)
        .map(|b| b.grant.bytes)
        .sum::<u64>();
    // The first of equal ones: the inputs are sorted by id.
    let read =
        reserved()
            .filter(|b| !b.render)
            .fold(None::<&PlannedBudget>, |kept, b| match kept {
                Some(kept) if kept.grant.bytes >= b.grant.bytes => Some(kept),
                _ => Some(b),
            });
    (
        render,
        read.map_or(0, |b| b.grant.bytes),
        read.map(|b| b.grant.entry.id),
    )
}

/// What the caches share of `total` once `baseline` and the budgets are
/// taken, and the allocator slack on it.
fn pool_of(total: u64, baseline: u64, budgets: &[PlannedBudget]) -> (u64, u64) {
    let (render, read, _) = reserve(budgets);
    let left = total
        .saturating_sub(baseline)
        .saturating_sub(render)
        .saturating_sub(read);
    let pool = mib_down(
        left / (100 + SLACK_PERCENT) * 100
            + left % (100 + SLACK_PERCENT) * 100 / (100 + SLACK_PERCENT),
    );
    (pool, left - pool)
}

fn expected(caches: &[Planned]) -> u64 {
    caches
        .iter()
        .fold(0u64, |sum, c| sum.saturating_add(c.grant.expected_bytes))
}

/// The largest `x` in `[0, 1]` at which `fits(x)` holds, `fits` monotone
/// (true at 0 if anywhere); `None` when it fails at 0.
fn largest(fits: impl Fn(f64) -> bool) -> Option<f64> {
    if !fits(0.0) {
        return None;
    }
    if fits(1.0) {
        return Some(1.0);
    }
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..64 {
        let mid = (lo + hi) / 2.0;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

/// `floor + x × (from − floor)`, in whole MiB down, never below `floor`.
fn between(floor: u64, from: u64, x: f64) -> u64 {
    let span = from.saturating_sub(floor) as f64;
    floor
        .saturating_add(mib_down((span * x) as u64))
        .min(from.max(floor))
}

/// A traffic-driven cache's size at `x` of the way to `x = 1`, the most
/// the weights reach: `base + x × λmax × weight`, between `lo` and `hi`.
fn weighted(c: &Planned, lo: u64, hi: u64, scale: f64, x: f64) -> u64 {
    let share = c.base as f64 + x * scale * c.weight as f64;
    mib_down(share.min(hi as f64) as u64).clamp(lo, hi.max(lo))
}

/// The weight scale at which every cache of `caches` reaches its `hi`.
fn full_scale(caches: &[&Planned], hi: impl Fn(&Planned) -> u64) -> f64 {
    caches
        .iter()
        .filter(|c| c.weight > 0)
        .map(|c| hi(c).saturating_sub(c.base) as f64 / c.weight as f64)
        .fold(0.0, f64::max)
}

/// Share `target` among the traffic-driven caches by weight, each between
/// `lo` and `hi`, as close to `target` as whole MiB allow without
/// exceeding it, when their `lo`s fit.
fn share(
    caches: &mut [Planned],
    group: impl Fn(&Planned) -> bool,
    lo: impl Fn(&Planned) -> u64,
    hi: impl Fn(&Planned) -> u64,
    target: u64,
) {
    let picked: Vec<usize> = (0..caches.len()).filter(|&i| group(&caches[i])).collect();
    let scale = full_scale(&picked.iter().map(|&i| &caches[i]).collect::<Vec<_>>(), &hi);
    let sum = |x: f64| {
        picked.iter().fold(0u64, |sum, &i| {
            let c = &caches[i];
            sum.saturating_add(weighted(c, lo(c), hi(c), scale, x))
        })
    };
    let x = largest(|x| sum(x) <= target).unwrap_or(0.0);
    for &i in &picked {
        let bytes = weighted(&caches[i], lo(&caches[i]), hi(&caches[i]), scale, x);
        caches[i].set(bytes);
    }
}

/// Cut the caches of `group` toward their floors, all by the same share of
/// what lies above them, until their expected use is `excess` less or
/// every one is at its floor.
fn cut_toward_floors(caches: &mut [Planned], group: Group, excess: u64) {
    let picked: Vec<usize> = (0..caches.len())
        .filter(|&i| caches[i].group == group)
        .collect();
    let from: Vec<u64> = picked.iter().map(|&i| caches[i].grant.bytes).collect();
    let at = |caches: &[Planned], x: f64| -> u64 {
        picked.iter().zip(&from).fold(0u64, |sum, (&i, &from)| {
            let c = &caches[i];
            sum.saturating_add(c.expected(between(c.grant.floor_bytes, from, x)))
        })
    };
    let now = at(caches, 1.0);
    let target = now.saturating_sub(excess);
    let x = largest(|x| at(caches, x) <= target).unwrap_or(0.0);
    for (&i, &from) in picked.iter().zip(&from) {
        let bytes = between(caches[i].grant.floor_bytes, from, x);
        caches[i].set(bytes);
    }
}

/// The plan for `inputs` (module docs).
pub fn plan(inputs: &Inputs) -> Plan {
    let total = inputs.total_bytes;
    let floors_only = total < MIN_TOTAL;
    let key = |e: &Entry| (e.id, e.scope.clone());
    let mut caches: Vec<Planned> = inputs
        .caches
        .iter()
        .map(|c| of_cache(c, floors_only))
        .collect();
    caches.sort_by(|a, b| key(&a.grant.entry).cmp(&key(&b.grant.entry)));
    let mut budgets: Vec<PlannedBudget> = inputs
        .budgets
        .iter()
        .map(|b| of_budget(b, total, floors_only))
        .collect();
    budgets.sort_by(|a, b| key(&a.grant.entry).cmp(&key(&b.grant.entry)));
    let collections = inputs.collections;
    let (outside, heap, measured) = match inputs.baseline {
        Baseline::Measured {
            outside_bytes,
            store_bytes,
        } => (
            outside_bytes.max(OUTSIDE_FLOOR),
            store_bytes.saturating_add(HEAP_PER_COLLECTION.saturating_mul(collections)),
            true,
        ),
        Baseline::Estimated => (
            ESTIMATED_BASELINE,
            ESTIMATED_PER_COLLECTION.saturating_mul(collections),
            false,
        ),
    };
    let baseline = outside.saturating_add(heap);
    let mut cut = false;

    if !floors_only {
        let (pool, _) = pool_of(total, baseline, &budgets);
        // Rule 4: the working sets, at most 35 % of what the caches that
        // no rule scales leave, then raised back to their size today.
        let settled = caches
            .iter()
            .filter(|c| matches!(c.group, Group::Never | Group::Demand))
            .fold(0u64, |sum, c| sum.saturating_add(c.grant.expected_bytes));
        let working_set = |c: &Planned| class(c.grant.entry.id) == Class::WorkingSet;
        let cap = percent(pool.saturating_sub(settled), WORKING_SET_PERCENT);
        let estimates = caches
            .iter()
            .filter(|c| working_set(c) && c.group != Group::Never)
            .fold(0u64, |sum, c| sum.saturating_add(c.want));
        for c in caches
            .iter_mut()
            .filter(|c| working_set(c) && c.group != Group::Never)
        {
            let scaled = if estimates > cap {
                let share = c.want as f64 * cap as f64 / estimates as f64;
                mib_down(share as u64).max(c.grant.floor_bytes)
            } else {
                c.want
            };
            c.set(scaled.max(c.today));
            c.snap();
            c.want = c.grant.bytes;
            if c.want != scaled && c.want == c.today {
                c.grant.reason = Reason::Today;
            }
        }
        // Rule 5 with rule 6: the traffic-driven caches share the rest,
        // none below its size today while that fits.
        let traffic = |c: &Planned| matches!(c.group, Group::Traffic | Group::Idle) && c.weight > 0;
        let others = caches
            .iter()
            .filter(|c| !traffic(c))
            .fold(0u64, |sum, c| sum.saturating_add(c.grant.expected_bytes));
        let left = pool.saturating_sub(others);
        let wants = caches
            .iter()
            .filter(|c| traffic(c))
            .fold(0u64, |sum, c| sum.saturating_add(c.want));
        if wants <= left {
            share(&mut caches, traffic, |c| c.want, |c| c.ceiling, left);
            for c in caches.iter_mut().filter(|c| traffic(c)) {
                if c.grant.bytes == c.today && c.today > c.grant.floor_bytes {
                    c.grant.reason = Reason::Today;
                }
            }
        }
        // Rule 7: what does not fit is cut, step by step.
        for step in [
            Step::Idle,
            Step::Traffic,
            Step::WorkingSet,
            Step::Budgets,
            Step::Demand,
        ] {
            let (pool, _) = pool_of(total, baseline, &budgets);
            let excess = expected(&caches).saturating_sub(pool);
            if excess == 0 {
                break;
            }
            cut = true;
            match step {
                Step::Traffic => {
                    let now = caches
                        .iter()
                        .filter(|c| c.group == Group::Traffic)
                        .fold(0u64, |sum, c| sum.saturating_add(c.grant.bytes));
                    share(
                        &mut caches,
                        |c| c.group == Group::Traffic,
                        |c| c.grant.floor_bytes,
                        |c| c.want,
                        now.saturating_sub(excess),
                    );
                }
                Step::Budgets => cut_budgets(&mut budgets, total, baseline, &caches),
                Step::Idle => cut_toward_floors(&mut caches, Group::Idle, excess),
                Step::WorkingSet => cut_toward_floors(&mut caches, Group::WorkingSet, excess),
                Step::Demand => cut_toward_floors(&mut caches, Group::Demand, excess),
            }
        }
        for c in caches.iter_mut() {
            c.snap();
            if let Some(step) = c.group.step() {
                if c.grant.bytes < c.want {
                    c.grant.reason = Reason::Cut(step);
                }
            }
        }
        for b in budgets.iter_mut() {
            if b.cut && b.grant.bytes < b.want {
                b.grant.reason = Reason::Cut(Step::Budgets);
            }
        }
    }

    let (pool, slack) = pool_of(total, baseline, &budgets);
    let (render, read, read_budget) = reserve(&budgets);
    let expected_bytes = expected(&caches);
    let needed = baseline
        .saturating_add(render)
        .saturating_add(read)
        .saturating_add(expected_bytes)
        .saturating_add(percent(expected_bytes, SLACK_PERCENT));
    let sum = |f: &dyn Fn(&Planned) -> Option<u64>| {
        caches
            .iter()
            .filter_map(f)
            .fold(0u64, |sum, bytes| sum.saturating_add(bytes))
    };
    let reachable = |c: &Planned| !matches!(c.grant.reason, Reason::Unreachable(_));
    Plan {
        total_bytes: total,
        reserves: Reserves {
            outside_bytes: outside,
            heap_bytes: heap,
            measured,
            render_bytes: render,
            read_bytes: read,
            read_budget,
            slack_bytes: slack,
        },
        pool_bytes: pool,
        expected_bytes,
        demand_bytes: sum(&|c| c.demand_bytes.filter(|_| reachable(c))),
        floor_bytes: sum(&|c| (c.group != Group::Never).then_some(c.grant.floor_bytes)),
        pinned_bytes: sum(&|c| (c.grant.reason == Reason::Pinned).then_some(c.grant.bytes)),
        shortfall_bytes: if floors_only || cut {
            needed.saturating_sub(total)
        } else {
            0
        },
        cut,
        caches: caches.into_iter().map(|c| c.grant).collect(),
        budgets: budgets.into_iter().map(|b| b.grant).collect(),
    }
}

/// Cut the budgets toward their floors, all by the same share, until the
/// pool they leave holds the caches' expected use, or every one is at its
/// floor.
fn cut_budgets(budgets: &mut [PlannedBudget], total: u64, baseline: u64, caches: &[Planned]) {
    let use_ = expected(caches);
    let from: Vec<u64> = budgets.iter().map(|b| b.grant.bytes).collect();
    let apply = |budgets: &mut [PlannedBudget], x: f64| {
        for (b, &from) in budgets.iter_mut().zip(&from) {
            if b.cut {
                let bytes = between(b.grant.floor_bytes, from, x);
                b.set(bytes);
            }
        }
    };
    let fits = |x: f64| {
        let mut trial: Vec<PlannedBudget> = budgets
            .iter()
            .map(|b| PlannedBudget {
                grant: b.grant.clone(),
                ..*b
            })
            .collect();
        apply(&mut trial, x);
        pool_of(total, baseline, &trial).0 >= use_
    };
    let x = largest(fits).unwrap_or(0.0);
    apply(budgets, x);
}

#[cfg(test)]
mod tests;
