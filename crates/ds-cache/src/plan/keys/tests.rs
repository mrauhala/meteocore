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
        unstage();
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
    // is marked the plan's to set.
    let bounds = Bounds::mib(8, 16);
    let changed = set(&[(key(), Size::Bounded(bounds))], true);
    assert_eq!(
        (
            changed[0].to_bytes,
            changed[0].configured_bytes,
            changed[0].deferred
        ),
        (64 * MIB, 16 * MIB, true)
    );
    assert_eq!(sized(), (64, 16, "plan:auto".into()));
    assert_eq!(
        row().entry.source,
        SizeSource::Bounds("[memory.caches] lightning_strike", bounds)
    );
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

/// #1313 review: report mode gives a cache a key set its documented size,
/// whatever auto left it at: a range auto left to the plan, at a size
/// inside or outside the range, and a key removed in auto. Auto marks what
/// it leaves to the plan as the plan's.
#[test]
fn report_mode_gives_a_keyed_cache_its_documented_size_after_auto() {
    let _serial = serial();
    let _reset = Reset;
    let cache = build(); // default 32 MiB

    // A pin, then a range in auto, kept at the pin's size for the plan.
    set(&[(key(), Size::Pinned(16))], false);
    let range = Bounds::mib(8, 64);
    let changed = set(&[(key(), Size::Bounded(range))], true);
    assert!(changed[0].deferred);
    assert_eq!(cache.capacity_bytes(), 16 * MIB);
    assert!(row().planned, "the plan's to move");
    assert_eq!(row().source(), SizeSource::Planned);
    // The same range in report: the default clamped into it.
    set(&[(key(), Size::Bounded(range))], false);
    assert_eq!(
        sized(),
        (32, 32, "bounds:[memory.caches] lightning_strike".into())
    );
    assert!(!row().planned);

    // A pin above a range's ceiling, the range in auto, then report.
    set(&[(key(), Size::Pinned(64))], false);
    let low = Bounds::mib(8, 16);
    set(&[(key(), Size::Bounded(low))], true);
    assert_eq!(cache.capacity_bytes(), 64 * MIB);
    set(&[(key(), Size::Bounded(low))], false);
    assert_eq!(
        sized(),
        (16, 16, "bounds:[memory.caches] lightning_strike".into())
    );

    // A pin removed in auto, then report: back to the default.
    set(&[(key(), Size::Pinned(48))], false);
    set(&[], true);
    assert_eq!(cache.capacity_bytes(), 48 * MIB);
    assert_eq!(row().source(), SizeSource::Planned);
    set(&[], false);
    assert_eq!(sized(), (32, 32, "default".into()));
    assert!(set(&[], false).is_empty(), "nothing left to give back");
    drop(cache);
}

/// #1313 review: a load's staged keys answer `pending_bytes` and change
/// nothing else: no size, no accepted value, no `env_size`. `set` takes a
/// load's values and drops the staged ones; `unstage` drops them.
#[test]
fn staged_keys_answer_pending_bytes_and_change_nothing() {
    let _serial = serial();
    let _reset = Reset;
    let cache = build(); // default 32 MiB
    let live = cache.capacity_bytes();
    assert_eq!(
        pending_bytes("lightning_strike", live),
        live,
        "nothing staged"
    );

    stage(&[(key(), Size::Pinned(64))], false);
    assert_eq!(pending_bytes("lightning_strike", live), 64 * MIB);
    assert_eq!(cache.capacity_bytes(), live, "nothing resized");
    assert_eq!(
        plan::env_size(VAR, 32),
        (32, SizeSource::Default),
        "nothing accepted"
    );
    assert_eq!(
        pending_bytes("pvol_pixel", 7),
        7,
        "a key the load leaves alone"
    );
    unstage();
    assert_eq!(pending_bytes("lightning_strike", live), live);

    // A range: its default clamped in report; where the plan sizes it, the
    // capacity now, 32 MiB, clamped into it.
    let range = Bounds::mib(40, 50);
    stage(&[(key(), Size::Bounded(range))], false);
    assert_eq!(pending_bytes("lightning_strike", live), 40 * MIB);
    stage(&[(key(), Size::Bounded(Bounds::mib(8, 50)))], true);
    assert_eq!(pending_bytes("lightning_strike", live), 32 * MIB);
    stage(&[(key(), Size::Bounded(range))], true);
    assert_eq!(pending_bytes("lightning_strike", live), 40 * MIB);

    // Accepted, then dropped by the next load: back to the default, or in
    // auto the plan's live size.
    set(&[(key(), Size::Pinned(64))], false);
    assert_eq!(
        pending_bytes("lightning_strike", 64 * MIB),
        64 * MIB,
        "set unstaged"
    );
    stage(&[], false);
    assert_eq!(pending_bytes("lightning_strike", 64 * MIB), 32 * MIB);
    stage(&[], true);
    assert_eq!(pending_bytes("lightning_strike", 64 * MIB), 64 * MIB);
    unstage();
    drop(cache);
}

/// #1313 review round 2: `pending_bytes` answers what `set` then gives the
/// cache, across a reload that keeps its values but switches auto → report
/// (or loses its total) after the plan moved the cache: a range kept, and
/// a key removed earlier from a keyed entry.
#[test]
fn pending_bytes_matches_set_across_a_mode_switch() {
    let _serial = serial();
    let _reset = Reset;
    let cache = build(); // default 32 MiB
    let range = Bounds::mib(8, 64);
    for (label, values) in [
        ("same range", vec![(key(), Size::Bounded(range))]),
        ("same unset key, keyed entry", vec![]),
    ] {
        // Accepted in auto; the plan grows the cache to 48 MiB.
        set(&[(key(), Size::Pinned(20))], false);
        set(&values, true);
        plan::apply(&row().entry, 48 * MIB);
        assert_eq!(cache.capacity_bytes(), 48 * MIB);
        // The next load: the same values, report mode.
        stage(&values, false);
        let pending = pending_bytes("lightning_strike", cache.capacity_bytes());
        set(&values, false);
        assert_eq!(
            (pending, cache.capacity_bytes()),
            (32 * MIB, 32 * MIB),
            "{label}"
        );
    }
    drop(cache);
}

/// #1313 review round 2: over many transitions, a keyed entry follows its
/// rule after each `set` (where the size applies, the configured size; in
/// auto otherwise, its capacity, marked the plan's when it is not the
/// configured size), and `pending_bytes`, asked before each `set` with the
/// same values and mode, answers the capacity `set` gives it, or where the
/// plan sizes it, that capacity clamped into a range's bounds. The plan is
/// simulated with `plan::apply`.
#[test]
fn a_keyed_entry_and_pending_bytes_follow_set_over_many_transitions() {
    let _serial = serial();
    let _reset = Reset;
    let cache = build(); // default 32 MiB, one shard
    type Values = Vec<(&'static Key, Size)>;
    let pin = |mb| vec![(key(), Size::Pinned(mb))];
    let range = |a, b| vec![(key(), Size::Bounded(Bounds::mib(a, b)))];
    let none: Values = vec![];
    // (step, values, plan_sized, the plan's grant afterwards in auto)
    type Step = (&'static str, Values, bool, Option<u64>);
    let steps: Vec<Step> = vec![
        ("report pin 16", pin(16), false, None),
        ("auto range 8..64", range(8, 64), true, Some(40)),
        ("auto removed", none.clone(), true, Some(24)),
        ("report removed", none.clone(), false, None),
        ("report pin 48", pin(48), false, None),
        ("auto pin 48", pin(48), true, None),
        ("auto, no total, range 8..16", range(8, 16), false, None),
        ("auto with total, range 8..16", range(8, 16), true, Some(12)),
        ("auto, no total again", range(8, 16), false, None),
        ("auto with total again", range(8, 16), true, Some(9)),
        ("report pin 20", pin(20), false, None),
        ("auto removed again", none.clone(), true, None),
        ("report removed again", none.clone(), false, None),
        ("auto pin 40", pin(40), true, None),
        ("auto range 50..60, above", range(50, 60), true, None),
        ("report range 50..60", range(50, 60), false, None),
    ];
    let mut bad = Vec::new();
    for (label, values, plan_sized, grant) in steps {
        let before = cache.capacity_bytes();
        stage(&values, plan_sized);
        let pending = pending_bytes("lightning_strike", before);
        set(&values, plan_sized);
        let r = row();
        let pinned = matches!(values.first(), Some((_, Size::Pinned(_))));
        let applies = !plan_sized || pinned;
        let capacity = r.usage.capacity_bytes;
        let follows = if applies {
            capacity == r.configured_bytes
        } else {
            capacity == before && r.planned == (capacity != r.configured_bytes)
        };
        let expected = match r.entry.source.bounds() {
            Some(bounds) if !applies => bounds.clamp(capacity),
            _ => capacity,
        };
        if !follows || pending != expected {
            bad.push(format!(
                "{label}: {} -> {} MiB, configured {} MiB, source {}, planned {}, pending {} \
                 MiB",
                before / MIB,
                capacity / MIB,
                r.configured_bytes / MIB,
                r.source(),
                r.planned,
                pending / MIB
            ));
        }
        if let Some(grant) = grant {
            plan::apply(&r.entry, grant * MIB);
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
    drop(cache);
}

/// `pending_bytes` of an entry no key ever set, whose capacity the plan
/// sized in auto: in report mode, the configured size `plan::restore` gives
/// it back; in auto, its capacity now, the plan's to move.
#[test]
fn pending_bytes_gives_an_unkeyed_plan_capacity_back_in_report() {
    let _serial = serial();
    let _reset = Reset;
    let cache = build(); // default 32 MiB
    plan::apply(&row().entry, 48 * MIB);
    assert!(row().planned);
    stage(&[], true);
    assert_eq!(pending_bytes("lightning_strike", 48 * MIB), 48 * MIB);
    stage(&[], false);
    assert_eq!(pending_bytes("lightning_strike", 48 * MIB), 32 * MIB);
    assert!(set(&[], false).is_empty(), "left to the plan's restore");
    assert_eq!(plan::restore(&row().entry), Some(32 * MIB));
    drop(cache);
}
