//! `[memory.caches]` (#1313): the one config home for the sizes of the
//! process-wide caches and admission budgets.
//!
//! The keys are `ds_cache::plan::keys::KEYS`. Here the server reads them
//! from the config ([`CacheSettings`], an unknown key failing the load),
//! warns where a size is set in two places or by a deprecated `MC_…_MB`
//! variable ([`log_warnings`]), and applies them:
//!
//! - the caches and budgets built from a variable take them through
//!   `ds_cache::plan::keys::set` ([`apply`]), at boot before the
//!   collections load and after every accepted reload: one built later is
//!   built at its key's size, one built before is resized in place;
//! - the render output caches the server builds itself (`metatile`,
//!   `rendered`, `vector_tile`) take them with their `[server]` aliases
//!   ([`RenderCacheSizes`]), resized in place on a reload as before.
//!
//! Precedence per cache: `[memory.caches]`, then the variable, then the
//! `[server]` alias, then the plan's size in `[memory] mode = "auto"`, then
//! the default. A key's `{ min, max }` is not a pin: the plan sizes the
//! cache within those bounds, and in `report` mode it takes its default
//! clamped into them.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Mutex;

use ds_cache::plan::keys::{self, Configured, Key, Size};
use ds_cache::plan::{Bounds, SizeSource};
use ds_cache::MIB;
use ds_core::config::{
    CacheSize, ServerConfig, DEFAULT_METATILE_CACHE_MB, DEFAULT_RENDERED_CACHE_MB,
};
use tracing::{info, warn};

use crate::admin::VECTOR_TILE_CACHE_MB;
use crate::cache_plan::mib;

/// The `[memory.caches]` values of a config, by key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheSettings(Vec<(&'static Key, Size)>);

impl CacheSettings {
    /// What `config`'s `[memory.caches]` sets. An unknown key is an error
    /// naming it and the valid keys: the load fails, at boot and on reload.
    pub fn of(config: &ServerConfig) -> Result<Self, String> {
        let mut unknown: Vec<&str> = config
            .memory
            .iter()
            .flat_map(|m| m.caches.keys())
            .filter(|key| keys::by_key(key).is_none())
            .map(String::as_str)
            .collect();
        if unknown.is_empty() {
            return Ok(Self::known(config));
        }
        unknown.sort_unstable();
        Err(format!(
            "[memory.caches]: unknown key{} {}; the keys are {}",
            if unknown.len() == 1 { "" } else { "s" },
            unknown.join(", "),
            keys::names()
        ))
    }

    /// [`Self::of`], leaving out an unknown key: for a config that passed
    /// it.
    pub fn known(config: &ServerConfig) -> Self {
        CacheSettings(
            config
                .memory
                .iter()
                .flat_map(|m| &m.caches)
                .filter_map(|(key, size)| Some((keys::by_key(key)?, plan_size(*size))))
                .collect(),
        )
    }

    /// The value set for `key`.
    pub fn get(&self, key: &str) -> Option<Size> {
        self.0.iter().find(|(k, _)| k.key == key).map(|(_, s)| *s)
    }
}

/// A config size as the plan reads it.
fn plan_size(size: CacheSize) -> Size {
    match size {
        CacheSize::Mib(mb) => Size::Pinned(mb),
        CacheSize::Range { min, max } => Size::Bounded(Bounds::mib(min, max)),
    }
}

/// A size in MiB and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sized {
    pub mb: u64,
    pub source: SizeSource,
}

/// The render output caches' sizes, which the server builds them at: the
/// meta-tile, rendered-image and vector-tile caches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCacheSizes {
    pub metatile: Sized,
    pub rendered: Sized,
    pub vector_tile: Sized,
}

impl RenderCacheSizes {
    /// The sizes `config` gives them: each its `[memory.caches]` key, else
    /// its `[server]` alias (the rendered-image cache then the deprecated
    /// `[wms] rendered_cache_mb`), else its default. The vector-tile cache
    /// has no alias: unset, its 128 MiB constant.
    pub fn of(config: &ServerConfig, settings: &CacheSettings) -> Self {
        let keyed = |key: &str, default_mb: u64| {
            let label = keys::by_key(key)?.label;
            let (bytes, source) = settings.get(key)?.sized(label, default_mb);
            Some(Sized {
                mb: bytes / MIB,
                source,
            })
        };
        let aliased = match config.server.metatile_cache_mb {
            Some(mb) => Sized {
                mb,
                source: SizeSource::Config("[server] metatile_cache_mb"),
            },
            None => Sized {
                mb: DEFAULT_METATILE_CACHE_MB,
                source: SizeSource::Default,
            },
        };
        let metatile = keyed("metatile", DEFAULT_METATILE_CACHE_MB).unwrap_or(aliased);
        let rendered = keyed("rendered", DEFAULT_RENDERED_CACHE_MB).unwrap_or_else(|| {
            let source = match config.rendered_cache_key() {
                Some(key @ "[server] rendered_cache_mb") => SizeSource::Config(key),
                Some(key) => SizeSource::Collection(key),
                None => SizeSource::Default,
            };
            Sized {
                mb: config.rendered_cache_mb().0,
                source,
            }
        });
        let vector_tile = keyed("vector_tile", VECTOR_TILE_CACHE_MB).unwrap_or(Sized {
            mb: VECTOR_TILE_CACHE_MB,
            source: SizeSource::Constant,
        });
        RenderCacheSizes {
            metatile,
            rendered,
            vector_tile,
        }
    }

    /// Meta-tile and rendered-image caches of `metatile` and `rendered`
    /// MiB, as set by `[server]`, and the vector-tile cache at its
    /// constant: for a load whose config is not at hand.
    #[cfg(test)]
    pub fn mb(metatile: u64, rendered: u64) -> Self {
        let server = |mb, key| Sized {
            mb,
            source: SizeSource::Config(key),
        };
        RenderCacheSizes {
            metatile: server(metatile, "[server] metatile_cache_mb"),
            rendered: server(rendered, "[server] rendered_cache_mb"),
            vector_tile: Sized {
                mb: VECTOR_TILE_CACHE_MB,
                source: SizeSource::Constant,
            },
        }
    }
}

/// A variable's value, set and not blank.
pub(crate) fn env_var(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `a`, `a and b`, `a, b and c`.
fn listed(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// The WARN lines about where `config` sets cache and budget sizes, for the
/// caller to log with each config it reads (#1313):
///
/// - one per cache or budget set in more than one place, naming the place
///   that sizes it and those ignored;
/// - the rendered-image cache's deprecated `[wms] rendered_cache_mb`, as
///   before, unless `[memory.caches] rendered` sizes it.
///
/// `env` reads a variable, set and not blank.
pub(crate) fn conflict_warnings(
    config: &ServerConfig,
    settings: &CacheSettings,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let rendered_keyed = settings.get("rendered").is_some();
    let mut warnings = Vec::new();
    for key in keys::KEYS {
        let mut places = Vec::new();
        if settings.get(key.key).is_some() {
            places.push(key.label.to_string());
        }
        if let Some(var) = key.env.filter(|var| env(var).is_some()) {
            places.push(var.to_string());
        }
        let alias_set = match key.key {
            "metatile" => config.server.metatile_cache_mb.is_some(),
            "rendered" => config.server.rendered_cache_mb.is_some(),
            _ => false,
        };
        if let Some(alias) = key.alias.filter(|_| alias_set) {
            places.push(alias.to_string());
        }
        if key.key == "rendered" && rendered_keyed {
            // The collections the deprecated fallback reads: those
            // serving WMS, Maps or Tiles.
            if config.deprecated_rendered_cache_mb().next().is_some() {
                places.push("the deprecated [wms] rendered_cache_mb".into());
            }
        }
        if places.len() > 1 {
            let ignored = &places[1..];
            warnings.push(format!(
                "{} is set in {} places: {} sizes it; {} {} ignored, remove {}",
                key.id,
                places.len(),
                places[0],
                listed(ignored),
                if ignored.len() == 1 { "is" } else { "are" },
                if ignored.len() == 1 { "it" } else { "them" },
            ));
        }
    }
    if !rendered_keyed {
        warnings.extend(config.rendered_cache_mb().1);
    }
    warnings
}

/// The WARN for each deprecated `MC_…_MB` size variable set whose key
/// `settings` leaves unset: it still sizes its cache until 0.18.0, and
/// names the key to set instead. A variable whose key is set is in
/// [`conflict_warnings`]. The retired ones say they are ignored.
pub(crate) fn variable_warnings(
    settings: &CacheSettings,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(&'static str, String)> {
    let mut warnings: Vec<(&'static str, String)> = keys::KEYS
        .iter()
        .filter(|key| settings.get(key.key).is_none())
        .filter_map(|key| {
            let var = key.env?;
            let value = env(var)?;
            let example = value
                .parse::<u64>()
                .map_or(String::new(), |mb| format!(" = {mb}"));
            Some((
                var,
                format!(
                    "{var} is deprecated since 0.16.0 and ignored from 0.18.0: set \
                     [memory.caches] {}{example} in config.toml instead",
                    key.key
                ),
            ))
        })
        .collect();
    warnings.sort();
    warnings
}

/// The variables [`log_warnings`] has warned about: each once per process,
/// whose environment does not change.
static WARNED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);

/// Log [`conflict_warnings`] for `config`, read at boot or by a reload,
/// and once per process each [`variable_warnings`].
pub(crate) fn log_warnings(config: &ServerConfig, settings: &CacheSettings) {
    for warning in conflict_warnings(config, settings, &env_var) {
        warn!("{warning}");
    }
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    let warned = warned.get_or_insert_with(HashSet::new);
    for (var, warning) in variable_warnings(settings, &env_var) {
        if warned.insert(var) {
            warn!("{warning}");
        }
    }
}

/// Apply `settings` to the caches and budgets built from a variable
/// (`ds_cache::plan::keys::set`): at boot before the collections load, and
/// after every accepted reload. `plan_sized`: `[memory] mode = "auto"` with
/// a total, so the plan fitted next applies the sizes a key leaves to it.
/// Logs one INFO block naming each size it changed.
pub(crate) fn apply(settings: &CacheSettings, plan_sized: bool) {
    let changed = keys::set(&settings.0, plan_sized);
    if let Some(block) = changes_block(&changed) {
        info!("{block}");
    }
    for warning in stopped_short_warnings(&changed) {
        warn!("{warning}");
    }
}

/// Stage `settings` for the load being built (`keys::stage`), before its
/// engines are: one that checks a process-wide size while it is built
/// checks the size the load gives it (`keys::pending_bytes`). [`apply`]
/// takes them once the load is accepted; dropping the guard drops them,
/// so a load rejected, or one that panics, leaves every size and value as
/// it was.
#[must_use = "dropping the guard unstages the values"]
pub(crate) fn stage(settings: &CacheSettings, plan_sized: bool) -> Staged {
    keys::stage(&settings.0, plan_sized);
    Staged
}

/// Unstages what [`stage`] staged when dropped: after an accepted load,
/// whose `apply` took the values already, it does nothing.
pub(crate) struct Staged;

impl Drop for Staged {
    fn drop(&mut self) {
        keys::unstage();
    }
}

/// The INFO block of what [`apply`] changed: each size, old → new, or what
/// it stays at, and its source; `None` when nothing changed.
pub(crate) fn changes_block(changed: &[Configured]) -> Option<String> {
    if changed.is_empty() {
        return None;
    }
    let mut out = format!(
        "[memory.caches] applied: {} size{} changed",
        changed.len(),
        if changed.len() == 1 { "" } else { "s" }
    );
    for change in changed {
        let _ = write!(out, "\n  {}", change.entry.id);
        if change.deferred {
            let _ = write!(
                out,
                " kept at {} for the memory plan",
                mib(change.from_bytes)
            );
        } else if change.to_bytes == change.from_bytes {
            let _ = write!(out, " stays at {}", mib(change.to_bytes));
        } else {
            let _ = write!(
                out,
                " {} → {}",
                mib(change.from_bytes),
                mib(change.to_bytes)
            );
        }
        let _ = write!(out, " ({}", change.entry.source);
        if let Some(bounds) = change.entry.source.bounds() {
            let _ = write!(
                out,
                ", floor {}, ceiling {}",
                mib(bounds.min_bytes),
                mib(bounds.max_bytes)
            );
        }
        if change.stopped_short() {
            let _ = write!(
                out,
                "; toward {}: its shrink stopped short",
                mib(change.configured_bytes)
            );
        }
        out.push(')');
    }
    Some(out)
}

/// What sets a size, in a WARN's words: `its key [memory.caches] k`, `its
/// range [memory.caches] k`, `its variable MC_X`, `its default`.
fn giver(source: &SizeSource) -> String {
    match source {
        SizeSource::Config(key) => format!("its key {key}"),
        SizeSource::Bounds(key, _) => format!("its range {key}"),
        SizeSource::Env(var) => format!("its variable {var}"),
        SizeSource::Default => "its default".into(),
        other => other.to_string(),
    }
}

/// The WARN for each size [`apply`] could not shrink to in place: a shrink
/// stops where the cache's shards still hold 32 entries each, and a
/// restart builds it at its key's size.
pub(crate) fn stopped_short_warnings(changed: &[Configured]) -> Vec<String> {
    changed
        .iter()
        .filter(|change| change.stopped_short())
        .map(|change| {
            format!(
                "{} stays at {}, above the {} {} gives it: a shrink in place stops where its \
                 shards still hold 32 entries each. A restart builds it at {}",
                change.entry.id,
                mib(change.to_bytes),
                mib(change.configured_bytes),
                giver(&change.entry.source),
                mib(change.configured_bytes)
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
