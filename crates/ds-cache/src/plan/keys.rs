//! `[memory.caches]` (#1313): one config key per process-wide cache and
//! admission budget, the one home for their sizes.
//!
//! [`KEYS`] names each key, the plan entry it sizes and the `MC_…_MB`
//! environment variable or `[server]` key it replaces. A key's value pins
//! the size, or leaves it to the memory plan within a floor and a ceiling
//! ([`Size`]). Precedence per cache: `[memory.caches]`, then the variable,
//! then the `[server]` alias, then the plan's size in `[memory] mode =
//! "auto"`, then the default.
//!
//! The server hands the accepted config's values to [`set`] at boot, before
//! the collections load, and after every accepted reload:
//!
//! - a cache or budget built later takes its size from them when it reads
//!   its variable ([`super::env_size`]), so it is built at that size;
//! - one already built is resized in place (#1311), and the plan lists it
//!   with the key as its source and the key's size as its configured one.
//!
//! The render output caches (`metatile`, `rendered`, `vector_tile`) have no
//! variable: the server builds them from the config it loads and sizes them
//! itself, as it did from `[server]`.

use std::sync::Mutex;

use super::{Bounds, Entry, Kind, SizeSource};
use crate::MIB;

/// One `[memory.caches]` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// The key: the `/metrics` family without its `_cache` suffix.
    pub key: &'static str,
    /// `[memory.caches] <key>`, as a size source names it.
    pub label: &'static str,
    /// The plan entry it sizes: its `/metrics` family.
    pub id: &'static str,
    pub kind: Kind,
    /// The `MC_…_MB` variable it replaces: deprecated since 0.16.0, ignored
    /// from 0.18.0.
    pub env: Option<&'static str>,
    /// The `[server]` key it is an alias of.
    pub alias: Option<&'static str>,
}

macro_rules! key {
    ($kind:ident $key:literal => $id:literal, env $env:literal) => {
        Key {
            key: $key,
            label: concat!("[memory.caches] ", $key),
            id: $id,
            kind: Kind::$kind,
            env: Some($env),
            alias: None,
        }
    };
    ($kind:ident $key:literal => $id:literal, alias $alias:literal) => {
        Key {
            key: $key,
            label: concat!("[memory.caches] ", $key),
            id: $id,
            kind: Kind::$kind,
            env: None,
            alias: Some($alias),
        }
    };
    ($kind:ident $key:literal => $id:literal) => {
        Key {
            key: $key,
            label: concat!("[memory.caches] ", $key),
            id: $id,
            kind: Kind::$kind,
            env: None,
            alias: None,
        }
    };
}

/// Every `[memory.caches]` key: caches, then budgets, each by key.
pub const KEYS: &[Key] = &[
    key!(Cache "geotiff_decoded_chunk" => "geotiff_decoded_chunk_cache",
        env "MC_GEOTIFF_DECODED_CHUNK_CACHE_MB"),
    key!(Cache "lightning_strike" => "lightning_strike_cache",
        env "MC_LIGHTNING_STRIKE_CACHE_MB"),
    key!(Cache "metatile" => "metatile_cache", alias "[server] metatile_cache_mb"),
    key!(Cache "netcdf_block" => "netcdf_block_cache", env "MC_NETCDF_BLOCK_CACHE_MB"),
    key!(Cache "netcdf_chunk" => "netcdf_chunk_cache", env "MC_NETCDF_CHUNK_CACHE_MB"),
    key!(Cache "netcdf_field" => "netcdf_field_cache", env "MC_NETCDF_FIELD_CACHE_MB"),
    key!(Cache "odim_composite" => "odim_composite_cache", env "MC_ODIM_COMPOSITE_CACHE_MB"),
    key!(Cache "pvol_cell_set" => "pvol_cell_set_cache", env "MC_PVOL_CELL_SET_CACHE_MB"),
    key!(Cache "pvol_pixel" => "pvol_pixel_cache", env "MC_PVOL_PIXEL_CACHE_MB"),
    key!(Cache "pvol_voxel_grid" => "pvol_voxel_grid_cache",
        env "MC_PVOL_VOXEL_GRID_CACHE_MB"),
    key!(Cache "rendered" => "rendered_cache", alias "[server] rendered_cache_mb"),
    key!(Cache "satellite_frame" => "satellite_frame_cache",
        env "MC_SATELLITE_FRAME_CACHE_MB"),
    key!(Cache "satellite_strip" => "satellite_strip_cache",
        env "MC_SATELLITE_STRIP_CACHE_MB"),
    key!(Cache "station_frame" => "station_frame_cache", env "MC_STATION_FRAME_CACHE_MB"),
    key!(Cache "station_grid" => "station_grid_cache", env "MC_STATION_GRID_CACHE_MB"),
    key!(Cache "tiles3d_content" => "tiles3d_content_cache",
        env "MC_3DTILES_CONTENT_CACHE_MB"),
    key!(Cache "vector_tile" => "vector_tile_cache"),
    key!(Budget "edr_response" => "edr_response", env "MC_EDR_RESPONSE_MEMORY_MB"),
    key!(Budget "geotiff_decode" => "geotiff_decode", env "MC_GEOTIFF_DECODE_MEMORY_MB"),
    key!(Budget "netcdf_read" => "netcdf_read", env "MC_NETCDF_READ_MEMORY_MB"),
    key!(Budget "pvol_read" => "pvol_read", env "MC_PVOL_READ_MEMORY_MB"),
    key!(Budget "render_budget" => "render_budget", env "MC_RENDER_MEMORY_MB"),
    key!(Budget "station_grid_compute" => "station_grid_compute",
        env "MC_STATION_GRID_MEMORY_MB"),
    key!(Budget "zarr_read" => "zarr_read", env "MC_ZARR_READ_MEMORY_MB"),
];

/// Size variables no longer read, which still WARN when set so an upgrade
/// never changes a size silently.
pub const RETIRED: &[&str] = &["MC_EDR_LOCATIONS_MEMORY_MB"];

/// The key named `key`.
pub fn by_key(key: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.key == key)
}

/// The key that replaces the variable `var`.
pub fn by_env(var: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.env == Some(var))
}

/// Every key, comma-separated, for an error naming the valid ones.
pub fn names() -> String {
    KEYS.iter().map(|k| k.key).collect::<Vec<_>>().join(", ")
}

/// What a key says of its cache or budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Size {
    /// A size in MiB, which pins it: the plan leaves it alone.
    Pinned(u64),
    /// Left to the plan within these bounds; the default clamped into them
    /// until the plan sizes it.
    Bounded(Bounds),
}

impl Size {
    /// The size in bytes this gives a cache whose default is `default_mb`,
    /// and its source, `key`'s.
    pub fn sized(self, key: &'static str, default_mb: u64) -> (u64, SizeSource) {
        match self {
            Size::Pinned(mb) => (mb.saturating_mul(MIB), SizeSource::Config(key)),
            Size::Bounded(bounds) => (
                bounds.clamp(default_mb.saturating_mul(MIB)),
                SizeSource::Bounds(key, bounds),
            ),
        }
    }
}

/// `[memory.caches]` values by key.
type Values = Vec<(&'static str, Size)>;

/// The values of the accepted config, as [`set`] last took them, by key.
static SETTINGS: Mutex<Values> = Mutex::new(Vec::new());

/// The default each variable was read with ([`super::env_size`]): what a
/// range clamps, whatever the variable says, since the key wins over it.
static DEFAULTS: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

pub(super) fn note_default(var: &'static str, default_mb: u64) {
    let mut defaults = DEFAULTS.lock().unwrap_or_else(|e| e.into_inner());
    if !defaults.iter().any(|(v, _)| *v == var) {
        defaults.push((var, default_mb));
    }
}

fn default_of(var: &str) -> Option<u64> {
    let defaults = DEFAULTS.lock().unwrap_or_else(|e| e.into_inner());
    defaults.iter().find(|(v, _)| *v == var).map(|(_, mb)| *mb)
}

fn setting(key: &str) -> Option<Size> {
    let settings = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    settings.iter().find(|(k, _)| *k == key).map(|(_, s)| *s)
}

/// The size in MiB and source the key that replaces `var` gives a cache
/// whose default is `default_mb`, when it is set: what [`super::env_size`]
/// returns instead of the variable's.
pub(super) fn configured(var: &str, default_mb: u64) -> Option<(u64, SizeSource)> {
    let key = by_env(var)?;
    let (bytes, source) = setting(key.key)?.sized(key.label, default_mb);
    Some((bytes / MIB, source))
}

/// A size [`set`] changed: a cache or budget given its key's size, or back
/// the size its variable or default gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    /// As the plan now lists it: with its new source.
    pub entry: Entry,
    /// The capacity before, and after.
    pub from_bytes: u64,
    pub to_bytes: u64,
    /// The size its settings now give it.
    pub configured_bytes: u64,
    /// Left to the memory plan fitted next, at its capacity: a range or an
    /// unset key in `[memory] mode = "auto"`.
    pub deferred: bool,
}

impl Configured {
    /// Resized toward its key's size, and its shrink stopped short of it:
    /// a shrink in place stops where the cache's shards still hold 32
    /// entries each, and only a restart builds it at that size.
    pub fn stopped_short(&self) -> bool {
        !self.deferred && self.to_bytes != self.configured_bytes
    }
}

/// The values of a load being built, which [`set`] takes once it is
/// accepted ([`stage`]), with whether the plan applies its sizes: what
/// [`pending_bytes`] answers from.
static STAGED: Mutex<Option<(Values, bool)>> = Mutex::new(None);

/// Stage the `[memory.caches]` values of a load being built, before its
/// engines are: one that checks a process-wide size while it is built
/// reads it through [`pending_bytes`], so a reload that raises the size
/// builds it against the size it will have, and one that lowers it against
/// that too. Nothing is resized and [`super::env_size`] still reads the
/// accepted values: a rejected load ([`unstage`]) leaves every size and
/// value as it was. [`set`] takes the accepted load's values and drops
/// these.
pub fn stage(values: &[(&'static Key, Size)], plan_sized: bool) {
    *STAGED.lock().unwrap_or_else(|e| e.into_inner()) = Some((
        values.iter().map(|(key, size)| (key.key, *size)).collect(),
        plan_sized,
    ));
}

/// Drop what [`stage`] staged: the load was rejected.
pub fn unstage() {
    *STAGED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The capacity the cache or budget of `key`, of `live_bytes` now, has
/// once the load being built is accepted ([`stage`]), from the very
/// decision [`set`] makes for it ([`decide`]), with the staged values and
/// mode, and the entry's own state: whether a key set it, and whether its
/// capacity is the plan's. Where `set` resizes it, the size it resizes it
/// to; where `set` leaves it to the plan fitted after the load (`auto`),
/// its capacity now, clamped into a range's bounds, which no grant leaves;
/// where `set` leaves it alone, its capacity now, but the plan's capacity
/// of one no key ever set goes back to its configured size, as report mode
/// gives it back (`plan::restore`). What `auto` then grants is the plan's,
/// known only once it is fitted. `live_bytes` when nothing is staged, or
/// nothing is built as the key's entry.
pub fn pending_bytes(key: &str, live_bytes: u64) -> u64 {
    let staged = STAGED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (Some((values, plan_sized)), Some(key)) = (staged, by_key(key)) else {
        return live_bytes;
    };
    let Some(state) = super::key_state(key) else {
        return live_bytes;
    };
    let size = values.iter().find(|(k, _)| *k == key.key).map(|(_, s)| *s);
    match decide(key, size, plan_sized, &state) {
        Some(decision) if decision.resize => decision.configured,
        Some(decision) if decision.deferred => decision
            .source
            .bounds()
            .map_or(state.capacity, |bounds| bounds.clamp(state.capacity)),
        Some(_) => state.capacity,
        None if !plan_sized && state.planned => state.configured,
        None => state.capacity,
    }
}

/// What a built entry holds now, as [`decide`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct State {
    /// Where its size comes from now.
    pub current: SizeSource,
    /// The size its settings give it now.
    pub configured: u64,
    /// A key has set it since it was built.
    pub keyed: bool,
    /// Its capacity is the plan's ([`super::Row::planned`]).
    pub planned: bool,
    pub capacity: u64,
}

/// What [`set`] does to one built entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Decision {
    /// The source and size its settings give it from now on.
    pub source: SizeSource,
    pub configured: u64,
    /// Either differs from what it had.
    pub changed: bool,
    /// Resize it to `configured` now: the size moves, or its capacity is
    /// the plan's, where the size applies (report mode, auto without a
    /// total, or a pin).
    pub resize: bool,
    /// Mark its capacity the plan's: the size moves where the plan sizes
    /// it, `auto` with a range or an unset key.
    pub defer: bool,
    /// Its capacity is not `configured`, and the plan fitted next sizes it.
    pub deferred: bool,
}

/// The one decision [`set`] applies and [`pending_bytes`] answers from
/// (#1313): what `key`, set to `size` or unset, does to an entry in
/// `state`, `plan_sized` saying whether the plan applies its sizes. `None`
/// when `set` leaves the entry as it was registered: no key has set it,
/// none sets it now.
///
/// A key's size, else its variable's, else the default is the entry's
/// configured size. Where it applies (not `plan_sized`, or a pin), a size
/// that moves, or a capacity that is the plan's, is resized to it: so
/// report mode gives a keyed entry its documented size whatever `auto`
/// left it at. Where the plan sizes it, a capacity other than the
/// configured size is marked the plan's, for the plan to move, or report
/// mode to give back.
pub(super) fn decide(
    key: &Key,
    size: Option<Size>,
    plan_sized: bool,
    state: &State,
) -> Option<Decision> {
    let var = key.env?;
    let default_mb = default_of(var)?;
    let (configured, source) = match size {
        Some(size) => size.sized(key.label, default_mb),
        None => {
            let (mb, source) = super::from_env(var, default_mb);
            (mb.saturating_mul(MIB), source)
        }
    };
    // Built while the key was set: its registration names the key.
    let from_key = matches!(
        &state.current,
        SizeSource::Config(label) | SizeSource::Bounds(label, _) if *label == key.label
    );
    if size.is_none() && !from_key && !state.keyed {
        return None;
    }
    let applies = !plan_sized || super::grant::pinned(&source);
    let changed = state.current != source || state.configured != configured;
    let moves = state.capacity != configured && (changed || state.planned);
    Some(Decision {
        source,
        configured,
        changed,
        resize: applies && moves,
        defer: !applies && moves,
        deferred: !applies && state.capacity != configured,
    })
}

/// Take the accepted config's `[memory.caches]` values (#1313): every cache
/// or budget built from now on reads them ([`super::env_size`]), and each
/// one already built with a variable takes them in place. A key set gives
/// it the key's source and size; a key no longer set gives back what its
/// variable, else its default, says. `plan_sized`: `[memory] mode =
/// "auto"`, where a size the key leaves to the plan (a range, or unset with
/// no variable) is the plan's to apply, after this; otherwise every changed
/// size is applied here. Keys without a variable are the server's to apply
/// ([`Key::env`]). Returns what changed.
///
/// The server calls this under its reload lock, but a cache can be built on
/// a request thread at any time: one whose initializer read its size before
/// this and registers after it keeps the size it read, with its old key's
/// source, until the next `set`, which finds its configured size differs
/// and gives it the key's size then (the next reload, or restart).
pub fn set(values: &[(&'static Key, Size)], plan_sized: bool) -> Vec<Configured> {
    *SETTINGS.lock().unwrap_or_else(|e| e.into_inner()) =
        values.iter().map(|(key, size)| (key.key, *size)).collect();
    unstage();
    KEYS.iter()
        .filter_map(|key| {
            let size = values
                .iter()
                .find(|(k, _)| k.key == key.key)
                .map(|(_, s)| *s);
            configure_live(key, size, plan_sized)
        })
        .collect()
}

/// [`set`] for one key whose cache or budget is built.
fn configure_live(key: &Key, size: Option<Size>, plan_sized: bool) -> Option<Configured> {
    super::configure(key, size, plan_sized)
}

#[cfg(test)]
mod tests;
