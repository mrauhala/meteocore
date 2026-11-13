use std::collections::BTreeSet;
use std::path::Path;

use super::*;
use crate::plan::{self, serial, Scope};
use crate::ByteBoundedCache;

/// `"MC_…_CACHE_MB"` and `"MC_…_MEMORY_MB"` string literals in the Rust
/// sources under `dir`, but this table's own files and the target dirs.
fn size_variables(dir: &Path, found: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            if !name.starts_with("target") && !name.starts_with('.') {
                size_variables(&path, found);
            }
            continue;
        }
        let table = path.ends_with("plan/keys.rs") || path.ends_with("plan/keys/tests.rs");
        if !name.ends_with(".rs") || table {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (at, _) in text.match_indices("\"MC_") {
            let rest = &text[at + 1..];
            let Some(end) = rest.find('"') else { continue };
            let var = &rest[..end];
            let named = var
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
            if named && (var.ends_with("_CACHE_MB") || var.ends_with("_MEMORY_MB")) {
                found.insert(var.to_string());
            }
        }
    }
}

/// #1313 acceptance: every `MC_*_CACHE_MB` and `MC_*_MEMORY_MB` size
/// variable the workspace reads has a `[memory.caches]` key, and every
/// key's variable is one the workspace reads. Test-only variables and the
/// retired ones are not sizes.
#[test]
fn every_size_variable_has_a_key_and_every_key_its_variable() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut read = BTreeSet::new();
    size_variables(&crates, &mut read);
    read.retain(|var| !var.starts_with("MC_TEST_") && !RETIRED.contains(&var.as_str()));
    let keyed: BTreeSet<String> = KEYS
        .iter()
        .filter_map(|k| k.env)
        .map(String::from)
        .collect();
    let unkeyed: Vec<&String> = read.difference(&keyed).collect();
    let unread: Vec<&String> = keyed.difference(&read).collect();
    assert!(
        unkeyed.is_empty() && unread.is_empty(),
        "variables without a [memory.caches] key: {unkeyed:?}; keys whose variable nothing \
         reads: {unread:?}"
    );
    assert!(read.len() >= 21, "the scan found the sources: {read:?}");
}

/// Keys are named after their `/metrics` family: a cache's without its
/// `_cache` suffix, a budget's as it is. Each is unique, and so is the
/// entry it sizes.
#[test]
fn keys_are_named_after_their_metrics_family() {
    let mut keys = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for key in KEYS {
        let name = match key.kind {
            Kind::Cache => key.id.strip_suffix("_cache").unwrap(),
            Kind::Budget => key.id,
        };
        assert_eq!(key.key, name, "{key:?}");
        assert_eq!(key.label, format!("[memory.caches] {}", key.key));
        assert!(keys.insert(key.key) && ids.insert(key.id), "{key:?}");
        assert_eq!(by_key(key.key), Some(key));
        if let Some(var) = key.env {
            assert_eq!(by_env(var), Some(key));
        }
    }
    assert!(names().starts_with("geotiff_decoded_chunk, lightning_strike, metatile"));
}

const VAR: &str = "MC_LIGHTNING_STRIKE_CACHE_MB";

fn key() -> &'static Key {
    by_key("lightning_strike").unwrap()
}

fn weigh(_: &u64, _: &u64) -> u64 {
    MIB
}

/// The key's cache as its static builds it: sized from its variable.
fn build() -> ByteBoundedCache<u64, u64> {
    let (bytes, source) = plan::env_bytes(VAR, 32);
    ByteBoundedCache::new(bytes, MIB, weigh).registered(
        "lightning_strike_cache",
        Scope::Engine("postgis"),
        source,
    )
}

fn row() -> plan::Row {
    plan::snapshot()
        .into_iter()
        .find(|row| row.entry.id == "lightning_strike_cache")
        .unwrap()
}

/// (capacity, configured, source) of the key's cache, in MiB.
fn sized() -> (u64, u64, String) {
    let row = row();
    (
        row.usage.capacity_bytes / MIB,
        row.configured_bytes / MIB,
        row.source().to_string(),
    )
}

/// Leaves no setting behind for the other tests.
struct Reset;

impl Drop for Reset {
    fn drop(&mut self) {
        *SETTINGS.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
    }
}

/// #1313: a cache built after the server took the config is built at its
/// key's size, with the key as its source; a key wins over its variable,
/// and unset gives the variable, else the default, back.
#[test]
fn a_cache_built_later_takes_its_keys_size() {
    let _serial = serial();
    let _reset = Reset;
    set(&[(key(), Size::Pinned(64))], false);
    assert_eq!(
        plan::env_size(VAR, 32),
        (64, SizeSource::Config("[memory.caches] lightning_strike"))
    );
    let bounds = Bounds::mib(100, 200);
    set(&[(key(), Size::Bounded(bounds))], false);
    assert_eq!(
        plan::env_size(VAR, 32),
        (
            100,
            SizeSource::Bounds("[memory.caches] lightning_strike", bounds)
        ),
        "the default clamped into the range"
    );
    set(&[], false);
    assert_eq!(plan::env_size(VAR, 32), (32, SizeSource::Default));
}

/// #1313: a cache built before takes a key in place, back its variable's
/// or default size when the key goes, and the plan lists the key as its
/// source with the key's size as its configured one. In `auto`, a size the
/// key leaves to the plan is the plan's to apply; a pin is applied in
/// either mode. Setting the same again changes nothing.
#[test]
fn a_built_cache_takes_its_key_in_place() {
    let _serial = serial();
    let _reset = Reset;
    let cache = build();
    assert_eq!(sized(), (32, 32, "default".into()));

    let changed = set(&[(key(), Size::Pinned(64))], true);
    assert_eq!(changed.len(), 1);
    assert_eq!(
        (changed[0].from_bytes, changed[0].to_bytes),
        (32 * MIB, 64 * MIB)
    );
    assert_eq!(
        changed[0].entry.source,
        SizeSource::Config("[memory.caches] lightning_strike")
    );
    assert_eq!(cache.capacity_bytes(), 64 * MIB, "the same cache, resized");
    assert_eq!(
        sized(),
        (64, 64, "config:[memory.caches] lightning_strike".into())
    );
    assert!(
        set(&[(key(), Size::Pinned(64))], true).is_empty(),
        "no change"
    );

    // A range in auto: the source and configured size move, the capacity
    // is the plan's to set.
    let bounds = Bounds::mib(8, 16);
    let changed = set(&[(key(), Size::Bounded(bounds))], true);
    assert_eq!(
        (changed[0].to_bytes, changed[0].configured_bytes),
        (64 * MIB, 16 * MIB)
    );
    assert_eq!(
        sized(),
        (64, 16, "bounds:[memory.caches] lightning_strike".into())
    );
    assert_eq!(row().entry.source.bounds(), Some(bounds));
    // In report mode it takes the default clamped into the range.
    set(&[], true);
    set(&[(key(), Size::Bounded(bounds))], false);
    assert_eq!(
        sized(),
        (16, 16, "bounds:[memory.caches] lightning_strike".into())
    );

    // Unset in report mode: back to the default.
    let changed = set(&[], false);
    assert_eq!(changed[0].entry.source, SizeSource::Default);
    assert_eq!(sized(), (32, 32, "default".into()));
    assert!(set(&[], false).is_empty(), "never set since: left alone");
    drop(cache);
}

/// A key unset hands its cache back to its variable, a pin that applies
/// in `auto` too.
#[test]
fn an_unset_key_gives_its_variable_back() {
    let _serial = serial();
    let _reset = Reset;
    let cache = build();
    std::env::set_var(VAR, "48");
    set(&[(key(), Size::Pinned(64))], true);
    assert_eq!(cache.capacity_bytes(), 64 * MIB, "the key wins");
    set(&[], true);
    std::env::remove_var(VAR);
    assert_eq!(
        sized(),
        (48, 48, format!("env:{VAR}")),
        "the variable pins it, applied in auto"
    );
    drop(cache);
}
