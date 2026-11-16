use std::time::Duration;

use ds_cache::plan::grant::{plan, Baseline, CacheInput, Inputs};
use ds_cache::plan::{CacheDemand, Demand, Entry, Scope, SizeSource};
use ds_cache::MIB;

use super::decide::*;
use super::*;
use crate::memory_plan::detect::{Limit, Reading};

const GIB: u64 = 1024 * MIB;

/// A demand of `bytes` from one collection, `entry_bytes` an entry.
fn demand(bytes: u64, entry_bytes: u64) -> Demand {
    let part = CacheDemand::demand(bytes, "rule", "setting").input("field_bytes", entry_bytes);
    Demand {
        bytes: part.bytes,
        rule: part.rule,
        floor: part.floor,
        ahead: part.ahead,
        setting: part.setting,
        parts: vec![("c".into(), part)],
    }
}

/// The plan's grant at a total of 16 GiB for a GRIB source's grid cache,
/// at its 256 MiB default, whose demand is 16 fields of `field_mb`.
fn grid_grant(field_mb: u64) -> u64 {
    let entry = Entry::cache(
        "grid_cache",
        Scope::Source {
            engine: "grib",
            id: "nwp".into(),
        },
        SizeSource::Default,
    );
    let inputs = Inputs {
        total_bytes: 16 * GIB,
        collections: 1,
        baseline: Baseline::Estimated,
        caches: vec![CacheInput {
            entry: entry.clone(),
            capacity_bytes: 256 * MIB,
            used_bytes: 0,
            entries: Some(0),
            demand: Some(demand(16 * field_mb * MIB, field_mb * MIB)),
            idle: false,
            unreachable: None,
            least: None,
        }],
        budgets: Vec::new(),
    };
    plan(&inputs).grant(&entry).unwrap().bytes
}

/// #1315 acceptance: the demand of a cache doubles ⇒ the re-plan tick
/// gives it its new grant within one tick; the demand jitters by 5 % ⇒ it
/// moves nothing. A grant moves a capacity only by more than 10 % of it and
/// 64 MiB.
#[test]
fn a_doubled_demand_moves_the_grant_within_one_tick_and_jitter_moves_nothing() {
    let applied = grid_grant(25);
    assert_eq!(applied, 460 * MIB, "16 × 25 MiB + 15 %");
    let doubled = grid_grant(50);
    assert!(doubled > 2 * applied - 64 * MIB, "{}", doubled / MIB);
    assert_eq!(
        tick_target(applied, doubled, 0, None, false, false),
        Some(doubled),
        "the next tick applies it"
    );
    for field_mb in [24, 26] {
        let jittered = grid_grant(field_mb);
        assert_ne!(jittered, applied);
        assert_eq!(
            tick_target(applied, jittered, 0, None, false, false),
            None,
            "{field_mb} MiB fields: 4 % jitter"
        );
    }
    // Both thresholds: 10 % alone, or 64 MiB alone, is not enough.
    assert!(!moves_enough(100 * MIB, 150 * MIB), "50 % but 50 MiB");
    assert!(
        !moves_enough(4 * GIB, 4 * GIB + 300 * MIB),
        "300 MiB but 7 %"
    );
    assert!(moves_enough(4 * GIB, 4 * GIB + 500 * MIB));
    assert!(moves_enough(0, 65 * MIB), "a cache off grows");
}

/// A resize that stopped short is not tried again while nothing it
/// depends on moved; the valve's ceiling caps a grant however little it
/// moves, but never below the floor; a lifted ceiling gives the grant at
/// once.
#[test]
fn the_tick_skips_a_refused_resize_and_follows_the_valves_ceiling() {
    assert_eq!(
        tick_target(300 * MIB, 100 * MIB, 0, None, false, true),
        None
    );
    assert_eq!(
        tick_target(300 * MIB, 100 * MIB, 0, None, false, false),
        Some(100 * MIB)
    );
    assert_eq!(
        tick_target(1000 * MIB, 1010 * MIB, 0, Some(900 * MIB), false, false),
        Some(900 * MIB)
    );
    assert_eq!(
        tick_target(900 * MIB, 1000 * MIB, 0, Some(950 * MIB), false, false),
        Some(950 * MIB),
        "a restore step of 5 %"
    );
    assert_eq!(
        tick_target(
            512 * MIB,
            3000 * MIB,
            2048 * MIB,
            Some(512 * MIB),
            false,
            false
        ),
        Some(2048 * MIB),
        "a ceiling never takes it below its floor"
    );
    assert_eq!(ceiled(3000 * MIB, 512 * MIB, 2048 * MIB), 2048 * MIB);
    assert_eq!(ceiled(1000 * MIB, 512 * MIB, 2048 * MIB), 1000 * MIB);
    assert_eq!(
        tick_target(950 * MIB, 1000 * MIB, 0, None, true, false),
        Some(1000 * MIB),
        "lifted"
    );
    // What a refused resize is not tried again with: any part moving
    // tries it again, a changed least or a lifted ceiling included.
    let refused = Refused {
        capacity: 300 * MIB,
        grant: 100 * MIB,
        least: 250 * MIB,
        capped: true,
    };
    assert_ne!(
        refused,
        Refused {
            least: 90 * MIB,
            ..refused
        }
    );
    assert_ne!(
        refused,
        Refused {
            capped: false,
            ..refused
        }
    );
}

/// A cache in the pressure simulation.
#[derive(Debug, Clone)]
struct Sim {
    entry: Entry,
    capacity: u64,
    grant: u64,
    /// Fills to its capacity, as a busy cache does; else holds its grant.
    fills: bool,
    flexible: bool,
}

fn sim(id: &'static str, gib: u64, flexible: bool) -> Sim {
    Sim {
        entry: Entry::cache(id, Scope::Process, SizeSource::Default),
        capacity: gib * GIB,
        grant: gib * GIB,
        fills: flexible,
        flexible,
    }
}

fn used(cache: &Sim) -> u64 {
    if cache.fills {
        cache.capacity
    } else {
        cache.grant.min(cache.capacity)
    }
}

/// #1315 acceptance: resident memory above the total ⇒ the flexible caches
/// shrink, the demand caches do not; back under ⇒ a slow restore, with no
/// oscillation over 50 ticks. The governor's decisions in a simulated
/// process: a total of 8 GiB, caches planned at 6 GiB (meta-tiles and
/// rendered images, which fill to their capacity, and satellite scans, a
/// demand cache), memory outside them 1 GiB but 3 GiB for 10 minutes.
#[test]
fn pressure_shrinks_the_flexible_caches_and_restores_them_slowly_without_oscillation() {
    let total = 8 * GIB;
    let mut caches = [
        sim("metatile_cache", 3, true),
        sim("rendered_cache", 1, true),
        sim("satellite_frame_cache", 2, false),
    ];
    let start = Instant::now();
    let mut pressure = Pressure::default();
    let mut caps: HashMap<Key, Ceiling> = HashMap::new();
    let mut relief_at = Vec::new();
    let mut history: Vec<Vec<u64>> = Vec::new();
    let samples = REPLAN_EVERY.as_secs() / SAMPLE_EVERY.as_secs();
    for tick in 0..50u64 {
        for sample in 0..samples {
            let secs = (tick * samples + sample) * SAMPLE_EVERY.as_secs();
            let now = start + Duration::from_secs(secs);
            let outside = if secs < 600 { 3 * GIB } else { GIB };
            let resident = outside + caches.iter().map(used).sum::<u64>();
            if pressure.observe(now, resident, total) == Signal::Relieve {
                relief_at.push(secs);
                let slots: Vec<Slot> = caches
                    .iter()
                    .map(|c| Slot {
                        entry: c.entry.clone(),
                        capacity: c.capacity,
                        used: used(c),
                        floor: 64 * MIB,
                        flexible: c.flexible,
                    })
                    .collect();
                for (key, target) in decide::relieve(&slots) {
                    caps.insert(
                        key.clone(),
                        Ceiling {
                            bytes: target,
                            source: SizeSource::Default,
                        },
                    );
                    let cache = caches
                        .iter_mut()
                        .find(|c| decide::key(&c.entry) == key)
                        .unwrap();
                    cache.capacity = target;
                }
            }
        }
        // The re-plan tick, its grants as planned.
        let now = start + Duration::from_secs((tick + 1) * REPLAN_EVERY.as_secs());
        let (_, lifted) = if pressure.may_restore(now) {
            let grants: HashMap<Key, u64> = caches
                .iter()
                .map(|c| (decide::key(&c.entry), c.grant))
                .collect();
            restore(&mut caps, &|key| grants.get(key).copied())
        } else {
            (Vec::new(), HashSet::new())
        };
        for cache in caches.iter_mut() {
            let key = decide::key(&cache.entry);
            if let Some(to) = tick_target(
                cache.capacity,
                cache.grant,
                64 * MIB,
                caps.get(&key).map(|c| c.bytes),
                lifted.contains(&key),
                false,
            ) {
                cache.capacity = to;
            }
        }
        history.push(caches.iter().map(|c| c.capacity).collect());
    }

    // Relieved while the pressure lasted, a minute apart, never after.
    assert!(relief_at.len() >= 2, "{relief_at:?}");
    assert!(relief_at.iter().all(|secs| *secs < 600), "{relief_at:?}");
    let low = history[1][0];
    assert!(
        low < 3 * GIB * 9 / 10,
        "metatile lowered: {} MiB",
        low / MIB
    );
    // The demand cache was never lowered.
    assert!(history.iter().all(|h| h[2] == 2 * GIB));
    // A slow restore: never more than 5 % of the grant (and 1 MiB) a tick,
    // never down again, and back at the plan within 50 ticks.
    for pair in history.windows(2).skip(2) {
        for (i, grant) in [3 * GIB, GIB].into_iter().enumerate() {
            let (before, after) = (pair[0][i], pair[1][i]);
            assert!(after >= before, "no oscillation: {history:?}");
            assert!(
                after - before <= grant / 20 + MIB,
                "{} → {}",
                before / MIB,
                after / MIB
            );
        }
    }
    assert_eq!(history.last().unwrap(), &vec![3 * GIB, GIB, 2 * GIB]);
    assert!(caps.is_empty());
    // Not before 10 minutes under 90 % of the total, and over several
    // ticks.
    let restored_at = history.iter().position(|h| h[0] == 3 * GIB).unwrap();
    let first_step = history.windows(2).position(|p| p[1][0] > p[0][0]).unwrap() + 1;
    assert!(first_step >= 3, "the first step at tick {first_step}");
    assert!(
        restored_at - first_step >= 4,
        "from tick {first_step} to {restored_at}"
    );
}

/// The valve's timers: over the total for a minute relieves once, then a
/// minute later again while it lasts; between 90 % and the total, neither
/// timer runs; under 90 % for 10 minutes allows a restore.
#[test]
fn the_valves_timers_follow_resident_memory() {
    let start = Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);
    let mut p = Pressure::default();
    assert_eq!(p.observe(at(0), 101, 100), Signal::Hold);
    assert_eq!(p.observe(at(45), 101, 100), Signal::Hold);
    assert_eq!(p.observe(at(60), 101, 100), Signal::Relieve);
    assert_eq!(
        p.observe(at(75), 101, 100),
        Signal::Hold,
        "a minute later again"
    );
    assert_eq!(p.observe(at(120), 101, 100), Signal::Relieve);
    assert_eq!(p.state(), "over");
    assert_eq!(p.observe(at(130), 95, 100), Signal::Hold);
    assert_eq!((p.state(), p.may_restore(at(2000))), ("normal", false));
    p.observe(at(200), 80, 100);
    assert!(!p.may_restore(at(799)));
    assert!(p.may_restore(at(800)));
    p.observe(at(801), 91, 100);
    assert!(
        !p.may_restore(at(2000)),
        "back over 90 %: the restore waits"
    );
}

/// Only flexible caches holding something are lowered, to 90 % of what
/// they hold in whole MiB, never below their floor; the valve spares a
/// cache its engines fill ahead of requests, a demand cache or a floor.
#[test]
fn the_valve_lowers_flexible_caches_to_90_percent_above_their_floor() {
    let slot = |id: &'static str, capacity: u64, used: u64, floor: u64, flexible: bool| Slot {
        entry: Entry::cache(id, Scope::Process, SizeSource::Default),
        capacity: capacity * MIB,
        used: used * MIB,
        floor: floor * MIB,
        flexible,
    };
    let key =
        |id: &'static str| decide::key(&Entry::cache(id, Scope::Process, SizeSource::Default));
    let lowered = decide::relieve(&[
        slot("metatile_cache", 4096, 2000, 512, true),
        slot("rendered_cache", 256, 100, 128, true),
        slot("vector_tile_cache", 128, 0, 16, true),
        slot("pvol_pixel_cache", 4096, 4000, 16, false),
    ]);
    assert_eq!(
        lowered,
        vec![
            (key("metatile_cache"), 1800 * MIB),
            (key("rendered_cache"), 128 * MIB),
        ]
    );
    let row = |id: &'static str, demand: Option<Demand>| Row {
        entry: Entry::cache(id, Scope::Process, SizeSource::Default),
        usage: ds_cache::plan::Usage::default(),
        window: None,
        demand,
        configured_bytes: 0,
        planned: false,
        least_bytes: 0,
    };
    assert!(spared(&row("satellite_frame_cache", None)));
    assert!(spared(&row("station_grid_cache", None)));
    let mut floor = demand(MIB, MIB);
    floor.floor = true;
    assert!(spared(&row("pvol_pixel_cache", Some(floor))));
    assert!(!spared(&row("metatile_cache", None)));
    assert!(!spared(&row("grid_cache", Some(demand(MIB, MIB)))));
}

/// A restore step raises each ceiling by 5 % of its grant, at least 1 MiB,
/// lifting the ceilings that reach their grant, and those of caches the
/// plan no longer grants.
#[test]
fn a_restore_step_raises_each_ceiling_by_5_percent_of_its_grant() {
    let key =
        |id: &'static str| decide::key(&Entry::cache(id, Scope::Process, SizeSource::Default));
    let ceiling = |bytes| Ceiling {
        bytes,
        source: SizeSource::Default,
    };
    let mut caps: HashMap<Key, Ceiling> = [
        (key("metatile_cache"), ceiling(1000 * MIB)),
        (key("rendered_cache"), ceiling(250 * MIB)),
        (key("gone_cache"), ceiling(10 * MIB)),
    ]
    .into();
    let grants: HashMap<Key, u64> = [
        (key("metatile_cache"), 2000 * MIB),
        (key("rendered_cache"), 256 * MIB),
    ]
    .into();
    let (raised, lifted) = restore(&mut caps, &|k| grants.get(k).copied());
    assert_eq!(raised, vec![key("metatile_cache")]);
    assert_eq!(caps[&key("metatile_cache")].bytes, 1100 * MIB);
    assert!(lifted.contains(&key("rendered_cache")) && lifted.contains(&key("gone_cache")));
    assert_eq!(caps.len(), 1);
}

fn change(id: &'static str, from: u64, to: u64) -> Change {
    Change {
        entry: Entry::cache(id, Scope::Process, SizeSource::Default),
        from_bytes: from * MIB,
        to_bytes: to * MIB,
        target_bytes: to * MIB,
        reason: "share by weight 45".into(),
        held: None,
        cleared: false,
    }
}

/// The value of `cache_plan_changes_total{cache}`.
fn changes_counted(cache: &str) -> u64 {
    crate::admin::REGISTRY
        .gather()
        .iter()
        .filter(|f| f.name() == "cache_plan_changes_total")
        .flat_map(|f| f.get_metric())
        .filter(|m| m.get_label().iter().any(|l| l.value() == cache))
        .map(|m| m.get_counter().get_value() as u64)
        .sum()
}

/// #1315 acceptance: every size change is logged, one line each with its
/// reason, and counted in `cache_plan_changes_total{cache}`; a change that
/// moved nothing is not counted.
#[test]
fn every_change_is_logged_and_counted() {
    init_counters();
    let changes = vec![
        change("governor_test_cache", 1024, 2048),
        change("governor_test_other_cache", 512, 256),
        change("governor_test_kept_cache", 64, 64),
    ];
    let ids = [
        "governor_test_cache",
        "governor_test_other_cache",
        "governor_test_kept_cache",
    ];
    let before = ids.map(changes_counted);
    memory_plan::count_changes(&changes);
    assert_eq!(
        ids.map(changes_counted),
        [before[0] + 1, before[1] + 1, before[2]]
    );
    let moved = &changes[..2];
    let block = tick_block(moved, 0, 0).unwrap();
    assert!(
        block.starts_with("Memory plan re-planned (every 5 minutes"),
        "{block}"
    );
    assert!(block.contains("\n  governor_test_cache 1024 MiB → 2048 MiB (share by weight 45)"));
    assert!(block.contains("\n  governor_test_other_cache 512 MiB → 256 MiB"));
    assert_eq!(tick_block(&[], 0, 0), None);
    let relief = relief_block(9 * GIB, 8 * GIB, &[], moved, &[], true);
    assert!(
        relief.starts_with(
            "Memory pressure: jemalloc holds 9216 MiB resident, above the total of 8192 MiB \
             for a minute: 2 caches lowered to 90 % of what they hold, dirty pages purged"
        ),
        "{relief}"
    );
    assert!(relief.contains("\n  governor_test_cache 1024 MiB → 2048 MiB"));
    let restored = tick_block(moved, 2, 1).unwrap();
    assert!(restored.contains(
        "2 caches the pressure valve lowered restored by 5 % of their grant, 1 back at it"
    ));
}

/// The valve's words when it could lower nothing: every flexible cache at
/// its floor, or each cache that refused its shrink named, with what a
/// restart does; and a refused shrink beside a relief.
#[test]
fn the_valve_names_the_caches_that_refuse_their_shrink() {
    let at_floors = stuck_block(9 * GIB, 8 * GIB, &[]);
    assert!(
        at_floors.starts_with(
            "Memory pressure: jemalloc still holds 9216 MiB resident, above the total of 8192 \
             MiB, and every flexible cache is at its floor"
        ),
        "{at_floors}"
    );
    let refused = Change {
        held: Some(Held::Refused),
        target_bytes: 810 * MIB,
        ..change("governor_test_stubborn_cache", 1024, 1024)
    };
    let stuck = stuck_block(9 * GIB, 8 * GIB, std::slice::from_ref(&refused));
    assert!(!stuck.contains("at its floor"), "{stuck}");
    assert!(
        stuck.contains(
            "the valve could lower nothing: 1 cache refused its shrink in place and keeps its \
             size, with no ceiling\n  governor_test_stubborn_cache kept at 1024 MiB"
        ),
        "{stuck}"
    );
    assert!(
        stuck.contains("toward 810 MiB: its resize stopped short"),
        "{stuck}"
    );
    assert!(stuck.contains("a restart reaches the size"), "{stuck}");
    let lowered = [change("governor_test_cache", 1024, 900)];
    let relief = relief_block(9 * GIB, 8 * GIB, &[], &lowered, &[refused], true);
    assert!(
        relief.contains(
            "\n  1 cache refused its shrink in place and keeps its size, with no ceiling:"
        ),
        "{relief}"
    );
    assert!(relief.contains("a restart reaches the size"), "{relief}");
}

/// The valve acts on `[memory] total_mb` or a cgroup limit's share, in
/// `auto`; never on a total taken from the host's memory, where the plan
/// still applies its sizes.
#[test]
fn the_valve_holds_only_a_configured_or_cgroup_total() {
    use ds_core::config::MemoryMode::{Auto, Report};
    let host = Reading {
        host_bytes: Some(64 * GIB),
        available_bytes: Some(32 * GIB),
        ..Reading::default()
    };
    let cgroup = Reading {
        limit: Some(Limit {
            version: 2,
            file: "memory.max",
            bytes: 10 * GIB,
        }),
        ..host.clone()
    };
    assert_eq!(
        memory_plan::valve_total_with(Auto, Some(4096), &host),
        Some(4 * GIB)
    );
    assert_eq!(
        memory_plan::valve_total_with(Auto, None, &cgroup),
        Some(8 * GIB)
    );
    assert_eq!(memory_plan::valve_total_with(Auto, None, &host), None);
    assert!(
        memory_plan::plans_sizes_with(Auto, None, &host),
        "the plan applies"
    );
    assert_eq!(
        memory_plan::valve_total_with(Report, Some(4096), &host),
        None
    );
    assert_eq!(
        memory_plan::valve_total_with(Auto, None, &Reading::default()),
        None
    );
}

/// Run the test `name` in a child process of its own, since it resizes the
/// process-wide caches and holds the governor's state: true in the child.
fn in_child(name: &str) -> bool {
    const CHILD: &str = "MC_TEST_GOVERNOR_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{name} failed in its child process");
    false
}

/// `[memory]` in `auto` with a configured total: the valve's gate is open.
const AUTO: &str = "[memory]\nmode = \"auto\"\ntotal_mb = 8192\n";

/// A config over the radar GeoTIFF fixture, its `[memory]` rewritten by
/// [`Fixture::write`].
struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl Fixture {
    fn new(memory: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fixture = Self { _dir: dir, path };
        fixture.write(memory);
        fixture
    }

    fn write(&self, memory: &str) {
        let radar = format!(
            "[[collections]]\nid = \"radar\"\ntitle = \"t\"\ndescription = \"d\"\n\
             engine_type = \"geotiff\"\ndata_path = {:?}\napis = [\"wms\", \"edr\"]\n\
             [collections.geotiff]\nfilename_template = \"radar_%Y%m%dT%H%MZ.tif\"\n\
             parameter = \"reflectivity\"\nunit = \"dBZ\"\n",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/radar")
        );
        std::fs::write(
            &self.path,
            format!("[server]\nhost = \"127.0.0.1\"\nport = 8000\n{memory}{radar}"),
        )
        .unwrap();
    }

    /// The server, booted.
    fn boot(&self) -> AdminState {
        let state = crate::watcher::tests::build_state(&self.path);
        cache_plan::log(&state, cache_plan::Occasion::Boot);
        state
    }
}

/// The plan registry's row of the process-wide cache `id`.
fn row(state: &AdminState, id: &str) -> Row {
    cache_plan::snapshot(state)
        .into_iter()
        .find(|row| row.entry.id == id)
        .unwrap()
}

/// The last plan's grant for `entry`.
fn grant_of(entry: &Entry) -> Grant {
    memory_plan::last()
        .and_then(|report| report.plan.as_ref()?.grant(entry).cloned())
        .unwrap()
}

/// The valve's ceiling of `bytes` on the cache of `row`, under its source.
fn hold(row: &Row, bytes: u64) -> Key {
    let key = decide::key(&row.entry);
    governor().caps.insert(
        key.clone(),
        Ceiling {
            bytes,
            source: row.entry.source.clone(),
        },
    );
    key
}

/// #1315: the governor's step on a running server. Gated in report mode,
/// where it takes no lock; in auto with `total_mb`, it samples resident
/// memory every step and fits the plan again 5 minutes after the last fit,
/// a reload's included; it skips while a reload holds `reload_lock`; and
/// `/admin/cache-plan` reports its last tick. Back in report mode it is
/// gated again, its ceilings dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_governor_is_gated_ticks_and_skips_while_a_reload_holds_the_lock() {
    if !in_child(
        "governor::tests::the_governor_is_gated_ticks_and_skips_while_a_reload_holds_the_lock",
    ) {
        return;
    }
    let fixture = Fixture::new("");
    let state = fixture.boot();
    let start = Instant::now();
    assert_eq!(step(&state, start), Step::Gated, "report mode");
    {
        let _reload = state.reload_lock.try_lock().unwrap();
        assert_eq!(
            step(&state, start),
            Step::Gated,
            "the gate is read before the lock"
        );
    }

    fixture.write(AUTO);
    crate::admin::do_reload(&state).unwrap();
    let now = Instant::now();
    assert_eq!(
        step(&state, now),
        Step::Ran {
            sampled: true,
            relieved: false,
            replanned: false,
        },
        "the reload fitted the plan just now"
    );
    let later = now + REPLAN_EVERY;
    assert_eq!(
        step(&state, later),
        Step::Ran {
            sampled: true,
            relieved: false,
            replanned: true,
        }
    );
    let document = json(&state);
    let tick = &document["last_tick"];
    assert!(tick["at"].is_string(), "{document}");
    assert_eq!(
        tick["changes"],
        json!([]),
        "nothing moved since the reload's own fit"
    );
    assert_eq!(document["pressure"]["valve"], true, "total_mb");
    assert_eq!(document["pressure"]["total_bytes"], 8192 * MIB);
    assert!(document["pressure"]["resident_bytes"].as_u64().unwrap() > 0);
    // #1314: the tick weighed elastic memory, report only. Nothing has a
    // window yet, so nothing is weighed and nothing moves.
    let elastic = &document["elastic"];
    assert_eq!(document["elastic_mode"], "report", "the default");
    assert_eq!(elastic["mode"], "report", "{document}");
    assert_eq!(elastic["held"], Value::Null);
    assert_eq!(elastic["moves"], json!([]));
    assert!(elastic["step_bytes"].as_u64().unwrap() >= ELASTIC_STEP_BYTES);

    {
        let _reload = state.reload_lock.try_lock().unwrap();
        assert_eq!(step(&state, later + REPLAN_EVERY), Step::Skipped);
    }
    // The skipped step fitted nothing: the next one does.
    assert!(matches!(
        step(&state, later + REPLAN_EVERY),
        Step::Ran {
            replanned: true,
            ..
        }
    ));

    hold(&row(&state, "metatile_cache"), 512 * MIB);
    fixture.write("");
    crate::admin::do_reload(&state).unwrap();
    assert!(governor().caps.is_empty(), "report mode drops the ceilings");
    assert_eq!(step(&state, later + 2 * REPLAN_EVERY), Step::Gated);
}

/// #1314 C: `[memory] elastic` is read again on every reload. Under
/// `apply` a reload keeps the loans whose caches it leaves as they were,
/// and its plan gives each cache its grant plus its offset; leaving `apply`
/// ends every loan, and the reload's plan gives each cache its grant; off
/// weighs nothing at the next tick; outside `mode = "auto"` it is off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_elastic_setting_is_read_on_every_reload() {
    if !in_child("governor::tests::the_elastic_setting_is_read_on_every_reload") {
        return;
    }
    let with = |elastic: &str| format!("{AUTO}elastic = \"{elastic}\"\n");
    let fixture = Fixture::new(&with("apply"));
    let state = fixture.boot();
    assert_eq!(json(&state)["elastic_mode"], "apply");
    let metatile = row(&state, "metatile_cache");
    let key = decide::key(&metatile.entry);
    let party = Party {
        key: key.clone(),
        source: metatile.entry.source.clone(),
        configured: metatile.configured_bytes,
    };
    let rendered = row(&state, "rendered_cache");
    let lender = Party {
        key: decide::key(&rendered.entry),
        source: rendered.entry.source.clone(),
        configured: rendered.configured_bytes,
    };
    governor()
        .ledger
        .book(Some(party.clone()), Some(lender.clone()), 64 * MIB);
    // A loan from the pool the reload's plan has no headroom for: shed
    // before the plan applies its grants (#1473 review).
    governor().ledger.book(Some(party), None, 4 * 8192 * MIB);
    crate::admin::do_reload(&state).unwrap();
    assert_eq!(governor().ledger.offset(&key), 64 * MIB as i64, "kept");
    assert_eq!(governor().ledger.pool_net(), 0, "the pool's loan shed");
    let grant = grant_of(&metatile.entry);
    assert_eq!(
        row(&state, "metatile_cache").usage.capacity_bytes,
        (grant.bytes + 64 * MIB).min(grant.ceiling_bytes),
        "its grant plus its offset"
    );

    fixture.write(&with("report"));
    crate::admin::do_reload(&state).unwrap();
    assert!(
        governor().ledger.is_empty(),
        "leaving apply ends every loan"
    );
    assert_eq!(
        row(&state, "metatile_cache").usage.capacity_bytes,
        grant.bytes
    );
    assert_eq!(json(&state)["elastic_mode"], "report");

    fixture.write(&with("off"));
    crate::admin::do_reload(&state).unwrap();
    let now = Instant::now() + REPLAN_EVERY;
    assert!(matches!(
        step(&state, now),
        Step::Ran {
            replanned: true,
            ..
        }
    ));
    let document = json(&state);
    assert_eq!(document["elastic_mode"], "off");
    assert_eq!(document["elastic"]["mode"], "off", "{document}");
    assert_eq!(document["elastic"]["caches"], json!([]));

    fixture.write("[memory]\nelastic = \"apply\"\n");
    crate::admin::do_reload(&state).unwrap();
    assert_eq!(json(&state)["elastic_mode"], "off", "only in auto");
}

/// #1315 review: a ceiling never holds a cache below the plan's floor nor
/// its range's `min`, and a reload that pins or re-bounds the cache drops
/// it. Before, a reload giving the meta-tile cache `{ min = 2048, max =
/// 4096 }` left it at an old 512 MiB ceiling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ceiling_never_holds_a_cache_below_its_floor_or_a_reloads_range() {
    if !in_child(
        "governor::tests::a_ceiling_never_holds_a_cache_below_its_floor_or_a_reloads_range",
    ) {
        return;
    }
    let fixture = Fixture::new(AUTO);
    let state = fixture.boot();
    let key = hold(&row(&state, "metatile_cache"), 512 * MIB);

    // A reload of the same config keeps the ceiling, and the cache under it.
    crate::admin::do_reload(&state).unwrap();
    let metatile = row(&state, "metatile_cache");
    let grant = grant_of(&metatile.entry);
    assert!(governor().caps.contains_key(&key));
    assert_eq!(
        metatile.usage.capacity_bytes,
        grant
            .bytes
            .min((512 * MIB).max(floor_of(&metatile.entry, &grant))),
        "grant {} MiB, floor {} MiB",
        grant.bytes / MIB,
        grant.floor_bytes / MIB
    );

    // The reload gives the cache a range from 2048 MiB: the ceiling set
    // under its old source goes.
    fixture.write(&format!(
        "{AUTO}[memory.caches]\nmetatile = {{ min = 2048, max = 4096 }}\n"
    ));
    crate::admin::do_reload(&state).unwrap();
    let metatile = row(&state, "metatile_cache");
    assert!(
        metatile.usage.capacity_bytes >= 2048 * MIB,
        "{} MiB from {}",
        metatile.usage.capacity_bytes / MIB,
        metatile.source()
    );
    assert!(!governor().caps.contains_key(&key), "superseded");

    // A ceiling under the range itself is raised to its floor, the min, in
    // a reload's plan and in a tick: the cache stops there, and so does
    // what /admin/cache-plan shows.
    let floor = || {
        let metatile = row(&state, "metatile_cache");
        floor_of(&metatile.entry, &grant_of(&metatile.entry))
    };
    hold(&metatile, 512 * MIB);
    crate::admin::do_reload(&state).unwrap();
    assert!(floor() >= 2048 * MIB);
    assert_eq!(governor().caps[&key].bytes, floor(), "a reload");
    assert_eq!(row(&state, "metatile_cache").usage.capacity_bytes, floor());
    hold(&metatile, 512 * MIB);
    let ticked = Instant::now() + REPLAN_EVERY;
    assert!(matches!(
        step(&state, ticked),
        Step::Ran {
            replanned: true,
            ..
        }
    ));
    assert_eq!(governor().caps[&key].bytes, floor(), "a tick");
    assert_eq!(row(&state, "metatile_cache").usage.capacity_bytes, floor());

    // Under 90 % for 10 minutes, the restore steps up from the floor: the
    // cache is back at its grant in the steps of 5 % that span the two,
    // not those from the old ceiling.
    let grant = grant_of(&row(&state, "metatile_cache").entry).bytes;
    let step_bytes = (grant / 100 * RESTORE_PERCENT).div_ceil(MIB) * MIB;
    let steps = (grant.saturating_sub(floor())).div_ceil(step_bytes).max(1);
    let from_old_ceiling = (grant - 512 * MIB).div_ceil(step_bytes);
    assert!(steps < from_old_ceiling, "{steps} vs {from_old_ceiling}");
    governor().pressure.under_since = Some(ticked);
    let mut at = ticked + UNDER_FOR;
    let mut taken = 0;
    while governor().caps.contains_key(&key) {
        at += REPLAN_EVERY;
        step(&state, at);
        taken += 1;
        assert!(taken <= steps, "{taken} restore steps, {steps} expected");
    }
    assert_eq!(
        row(&state, "metatile_cache").usage.capacity_bytes,
        grant_of(&row(&state, "metatile_cache").entry).bytes
    );
}

/// #1315 review: a recovery pass, which refits the accepted config, does
/// not put the tick off, and restores run on their own clock, which no fit
/// puts off. Before, `radar_recovery_loop` backing off to the tick's own 5
/// minutes kept the ceilings in place for as long as a source was down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restores_run_on_their_own_clock_and_recovery_passes_do_not_put_the_tick_off() {
    if !in_child(
        "governor::tests::restores_run_on_their_own_clock_and_recovery_passes_do_not_put_the_tick_off",
    ) {
        return;
    }
    let fixture = Fixture::new(AUTO);
    let state = fixture.boot();
    let booted = governor().fitted_at.unwrap();
    cache_plan::log(&state, cache_plan::Occasion::Recovery);
    assert_eq!(governor().fitted_at, Some(booted), "a recovery pass");
    crate::admin::do_reload(&state).unwrap();
    assert!(governor().fitted_at.unwrap() > booted, "a reload");

    // The valve lowered the meta-tile cache; the process has been under 90 %
    // of its total for 11 minutes, and a fit ran a second ago.
    let key = hold(&row(&state, "metatile_cache"), 512 * MIB);
    let at = Instant::now() + UNDER_FOR + Duration::from_secs(60);
    {
        let mut governor = governor();
        governor.pressure.under_since = Some(Instant::now());
        governor.fitted_at = Some(at - Duration::from_secs(1));
    }
    assert_eq!(
        step(&state, at),
        Step::Ran {
            sampled: true,
            relieved: false,
            replanned: true,
        },
        "the restore is due"
    );
    assert_eq!(governor().restored_at, Some(at));
    assert!(
        governor()
            .caps
            .get(&key)
            .is_none_or(|ceiling| ceiling.bytes > 512 * MIB),
        "raised by 5 % of the grant"
    );
    // The next restore is 5 minutes on.
    let soon = at + Duration::from_secs(60);
    assert!(matches!(
        step(&state, soon),
        Step::Ran {
            replanned: false,
            ..
        }
    ));
    assert!(matches!(
        step(&state, at + REPLAN_EVERY),
        Step::Ran {
            replanned: true,
            ..
        }
    ));
}

/// #1315 review: over the total with every flexible cache at its floor,
/// the valve no longer WARNs, purges and counts every minute: it WARNs at
/// most every 30 minutes, purges at most every 10, and counts only what it
/// did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_valve_at_its_floors_warns_purges_and_counts_sparingly() {
    if !in_child("governor::tests::the_valve_at_its_floors_warns_purges_and_counts_sparingly") {
        return;
    }
    let fixture = Fixture::new(AUTO);
    let state = fixture.boot();
    let count = |action: &str| PRESSURE_ACTIONS.with_label_values(&[action]).get();
    let (resident, total) = (9 * GIB, 8 * GIB);

    // The caches are empty: one relief takes them to their floors.
    let mut at = Instant::now();
    let mut reliefs = 0;
    while super::relieve(&state, resident, total, at) {
        reliefs += 1;
        at += OVER_FOR;
        assert!(reliefs < 3, "the valve keeps lowering");
    }
    assert_eq!(count("relieve"), reliefs);
    assert_eq!(governor().stuck_warned_at, Some(at), "the first WARN");
    let purged = governor().purged_at.unwrap();
    let purges = count("purge");

    // Still over a minute on: nothing to lower, purge, WARN or count.
    for later in [at + OVER_FOR, purged + PURGE_EVERY - Duration::from_secs(1)] {
        assert!(!super::relieve(&state, resident, total, later));
        assert_eq!(count("relieve"), reliefs);
        assert_eq!(count("purge"), purges);
        assert_eq!(governor().purged_at, Some(purged));
        assert_eq!(governor().stuck_warned_at, Some(at));
    }
    // 10 minutes on it purges once more, 30 on it WARNs again.
    assert!(!super::relieve(
        &state,
        resident,
        total,
        purged + PURGE_EVERY
    ));
    assert_eq!(governor().purged_at, Some(purged + PURGE_EVERY));
    assert!(count("purge") <= purges + 1, "jemalloc may refuse");
    assert_eq!(governor().stuck_warned_at, Some(at));
    assert!(!super::relieve(
        &state,
        resident,
        total,
        at + FLOORS_WARN_EVERY
    ));
    assert_eq!(governor().stuck_warned_at, Some(at + FLOORS_WARN_EVERY));
    assert_eq!(count("relieve"), reliefs);
}

/// #1315 review: where the valve's total goes while the plan still applies
/// its sizes, the ceilings go and the next tick gives those caches their
/// grant, however little it moves; a reload in between applies none of
/// them. Before, a reload into report mode and straight back brought an
/// old ceiling back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn released_ceilings_give_the_grant_and_no_reload_brings_them_back() {
    if !in_child("governor::tests::released_ceilings_give_the_grant_and_no_reload_brings_them_back")
    {
        return;
    }
    let fixture = Fixture::new(AUTO);
    let state = fixture.boot();
    let key = hold(&row(&state, "metatile_cache"), 512 * MIB);
    crate::admin::do_reload(&state).unwrap();

    // Report mode and straight back, with no step between.
    fixture.write("");
    crate::admin::do_reload(&state).unwrap();
    assert!(governor().caps.is_empty());
    fixture.write(AUTO);
    crate::admin::do_reload(&state).unwrap();
    let metatile = row(&state, "metatile_cache");
    let grant = grant_of(&metatile.entry).bytes;
    assert_eq!(metatile.usage.capacity_bytes, grant, "no stale ceiling");

    // A released cache 32 MiB under its grant gets it at the next tick,
    // which the thresholds alone would not give it.
    ds_cache::plan::apply(&metatile.entry, grant - 32 * MIB).unwrap();
    governor().released.insert(key);
    let at = Instant::now() + REPLAN_EVERY;
    assert!(matches!(
        step(&state, at),
        Step::Ran {
            replanned: true,
            ..
        }
    ));
    let metatile = row(&state, "metatile_cache");
    assert_eq!(
        metatile.usage.capacity_bytes,
        grant_of(&metatile.entry).bytes
    );
    assert!(governor().released.is_empty());
    ds_cache::plan::apply(&metatile.entry, grant - 32 * MIB).unwrap();
    step(&state, at + REPLAN_EVERY);
    assert_eq!(
        row(&state, "metatile_cache").usage.capacity_bytes,
        grant - 32 * MIB,
        "not released: the thresholds hold it"
    );
}

/// #1315 review: every size change is counted, a reload's resize of the
/// render caches by its `[server]` setting and of a GRIB engine's own by
/// its collection's included; a reload that changes no size counts none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reloads_resizes_are_counted() {
    if !in_child("governor::tests::a_reloads_resizes_are_counted") {
        return;
    }
    let fixture = Fixture::new("");
    let write = |metatile_mb: u64, grid_mb: u64| {
        fixture.write(&format!("metatile_cache_mb = {metatile_mb}\n"));
        let mut config = std::fs::read_to_string(&fixture.path).unwrap();
        config.push_str(&format!(
            "[[collections]]\nid = \"nwp\"\ntitle = \"t\"\ndescription = \"d\"\n\
             engine_type = \"grib\"\napis = [\"edr\", \"wms\"]\n[collections.grib]\n\
             data_path = {:?}\nindex_format = \"ecmwf-json\"\ngrid_cache_mb = {grid_mb}\n",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/grib-local")
        ));
        std::fs::write(&fixture.path, config).unwrap();
    };
    write(1024, 256);
    let state = fixture.boot();
    let counted = || {
        (
            changes_counted("metatile_cache"),
            changes_counted("grid_cache"),
        )
    };
    let (metatile, grid) = counted();

    write(512, 512);
    crate::admin::do_reload(&state).unwrap();
    assert_eq!(
        row(&state, "metatile_cache").usage.capacity_bytes,
        512 * MIB
    );
    assert_eq!(counted(), (metatile + 1, grid + 1));

    crate::admin::do_reload(&state).unwrap();
    assert_eq!(counted(), (metatile + 1, grid + 1), "nothing changed");
}

/// An in-memory sink for a `fmt` subscriber's lines.
#[derive(Clone, Default)]
struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// What `relieve` logs at `now`, and whether it lowered anything.
fn relieve_logged(state: &AdminState, resident: u64, total: u64, now: Instant) -> (bool, String) {
    let logs = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let lowered = tracing::subscriber::with_default(subscriber, || {
        super::relieve(state, resident, total, now)
    });
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    (lowered, text)
}

/// #1315 review: a cache that refuses its shrink in place keeps no ceiling,
/// and the valve's WARN names it and says what a restart does, never that
/// every cache is at its floor. Before, the ceiling was kept and the WARN
/// said the caches were at their floors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cache_refusing_its_shrink_keeps_no_ceiling_and_is_named() {
    if !in_child("governor::tests::a_cache_refusing_its_shrink_keeps_no_ceiling_and_is_named") {
        return;
    }
    // A cache holding 900 of its 1024 MiB whose resize never moves it. The
    // fixture has no other cache of this id.
    struct Stubborn;
    let owner = std::sync::Arc::new(Stubborn);
    let entry = Entry::cache("odim_composite_cache", Scope::Process, SizeSource::Default);
    ds_cache::plan::register_resizable(
        entry.clone(),
        &owner,
        |_| {
            ds_cache::plan::Usage::cache(
                ds_cache::CacheMetrics {
                    bytes: 900 * MIB,
                    capacity_bytes: 1024 * MIB,
                    ..Default::default()
                },
                Some(9),
            )
        },
        ds_cache::plan::Resize {
            to: |_, _| 1024 * MIB,
            least: |_| 0,
            clear: |_| {},
        },
    );
    let fixture = Fixture::new(AUTO);
    let state = fixture.boot();
    let relieves = || PRESSURE_ACTIONS.with_label_values(&["relieve"]).get();

    let at = Instant::now();
    let (lowered, logged) = relieve_logged(&state, 9 * GIB, 8 * GIB, at);
    assert!(!lowered);
    assert!(
        !governor().caps.contains_key(&decide::key(&entry)),
        "{logged}"
    );
    assert_eq!(relieves(), 0);
    assert!(logged.contains("WARN"), "{logged}");
    assert!(
        logged.contains("1 cache refused its shrink in place and keeps its size, with no ceiling"),
        "{logged}"
    );
    assert!(
        logged.contains("odim_composite_cache kept at 1024 MiB"),
        "{logged}"
    );
    assert!(logged.contains("a restart reaches the size"), "{logged}");
    assert!(!logged.contains("at its floor"), "{logged}");

    // A minute on, the same refusal: no WARN until 30 minutes have passed.
    let (_, logged) = relieve_logged(&state, 9 * GIB, 8 * GIB, at + OVER_FOR);
    assert!(!logged.contains("Memory pressure"), "{logged}");
    let (_, logged) = relieve_logged(&state, 9 * GIB, 8 * GIB, at + FLOORS_WARN_EVERY);
    assert!(logged.contains("refused its shrink"), "{logged}");
    assert!(!governor().caps.contains_key(&decide::key(&entry)));
    drop(owner);
}

// --- Elastic memory (#1314) ----------------------------------------------

use ds_cache::plan::{Activity, Kind, Usage};
use ds_cache::Churn;
use std::cell::RefCell;

/// What reaches a simulated cache: `lookups` an hour spread evenly over a
/// working set of `working_set` bytes, each miss costing `cost` seconds. A
/// cache of capacity C hits min(1, C / W) of them, one d × larger
/// min(1, (1 + d) C / W): the difference are its ghost hits at depth d,
/// cumulative by construction. A scan's keys are never asked for again: no
/// ghost hits however much it misses.
#[derive(Debug, Clone, Copy)]
struct Load {
    lookups: f64,
    working_set: u64,
    cost: f64,
    scan: bool,
}

const IDLE: Load = Load {
    lookups: 0.0,
    working_set: GIB,
    cost: 0.003,
    scan: false,
};

/// 100 000 lookups an hour over `gib` GiB, 3 ms a miss.
fn thrash(gib: u64) -> Load {
    Load {
        lookups: 100_000.0,
        working_set: gib * GIB,
        cost: 0.003,
        scan: false,
    }
}

#[derive(Debug, Clone)]
struct SimCache {
    key: Key,
    capacity: u64,
    /// The size its settings give it, what the plan fits it from.
    configured: u64,
    floor: u64,
    ceiling: u64,
    load: Load,
    lends: bool,
    need: Option<u64>,
    /// Filled ahead of its reads (`Churn::prewarmed`).
    prewarmed: bool,
}

fn sim_cache(id: &'static str, capacity_mib: u64, floor_mib: u64, load: Load) -> SimCache {
    SimCache {
        key: (Kind::Cache, id, Scope::Process),
        capacity: capacity_mib * MIB,
        configured: capacity_mib * MIB,
        floor: floor_mib * MIB,
        ceiling: 8 * capacity_mib * MIB,
        load,
        lends: true,
        need: None,
        prewarmed: false,
    }
}

/// A side of a move or a loan by id: `pool` for the pool's headroom.
fn id(key: &Option<Key>) -> &'static str {
    key.as_ref().map_or("pool", |key| key.1)
}

/// Rules with borrowing and no decay: the pure simulations'.
const BORROW: Rules = Rules {
    may_borrow: true,
    may_return: true,
    decays: false,
};

/// The bytes `caches` hold between them: a pool with no headroom.
fn held_by(caches: &[SimCache]) -> u64 {
    caches.iter().map(|c| c.capacity).sum()
}

/// An hour of `cache` at its capacity: hits, misses and cumulative ghost
/// hits by depth, the latter times `noise`.
fn hour_of(cache: &SimCache, noise: f64) -> (f64, f64, [f64; 3]) {
    let capacity = cache.capacity as f64;
    let working_set = cache.load.working_set as f64;
    let hit = |bytes: f64| (bytes / working_set).min(1.0);
    let hits = cache.load.lookups * hit(capacity);
    let ghost = if cache.load.scan {
        [0.0; 3]
    } else {
        ds_cache::plan::GHOST_DEPTH_FACTORS.map(|depth| {
            cache.load.lookups * (hit((1.0 + depth) * capacity) - hit(capacity)) * noise
        })
    };
    (hits, cache.load.lookups - hits, ghost)
}

/// `cache` as elastic memory weighs it over an hour, its ghost hits times
/// `noise`, as `elastic_inputs` weighs a row.
fn weigh(cache: &SimCache, noise: f64) -> Elastic {
    let (hits, misses, ghost) = hour_of(cache, noise);
    let (value, depth) = best_depth(
        cache.capacity,
        ghost,
        cache.load.cost,
        ds_cache::plan::GHOST_DEPTH_FACTORS,
    );
    Elastic {
        key: cache.key.clone(),
        capacity: cache.capacity,
        floor: cache.floor,
        ceiling: cache.ceiling,
        value,
        ghost_hits: ghost[depth] as u64,
        depth,
        borrows: misses > 0.0,
        lend_value: lend_value(value, hits, cache.load.cost, cache.capacity),
        lends: cache.lends,
        need: cache.need,
        grant: cache.configured,
        offset: cache.capacity as i64 - cache.configured as i64,
        used: cache.capacity.min(cache.load.working_set),
        idle: cache.load.lookups < 60.0,
        decays: cache.lends && !cache.prewarmed,
        pool_lent: 0,
    }
}

/// One decision over `caches`, its moves carried forward as if applied.
/// `noise(i)` scales cache `i`'s ghost hits.
fn sim_tick(caches: &mut [SimCache], pool: u64, noise: &mut dyn FnMut(usize) -> f64) -> Vec<Move> {
    let weighed: Vec<Elastic> = caches
        .iter()
        .enumerate()
        .map(|(i, cache)| weigh(cache, noise(i)))
        .collect();
    let expected: u64 = weighed.iter().map(|c| c.expected_at(c.capacity)).sum();
    let moves = elastic(&weighed, pool, pool.saturating_sub(expected), BORROW);
    for m in &moves {
        for cache in caches.iter_mut() {
            if Some(&cache.key) == m.borrower.as_ref() {
                cache.capacity += m.bytes;
            }
            if Some(&cache.key) == m.lender.as_ref() {
                cache.capacity -= m.bytes;
            }
            assert!(cache.capacity >= cache.floor, "{cache:?} below its floor");
            assert!(
                cache.capacity <= cache.ceiling,
                "{cache:?} above its ceiling"
            );
        }
    }
    moves
}

/// ±5 % noise, reproducible: a linear congruential generator.
fn noise(seed: u64) -> impl FnMut(usize) -> f64 {
    let mut state = seed;
    move |_| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        0.95 + 0.1 * ((state >> 11) as f64 / (1u64 << 53) as f64)
    }
}

/// `cache`'s plan row after an hour at its capacity, as the snapshot gives
/// it: its window, its lifetime counters, its configured size.
fn sim_row(cache: &SimCache) -> Row {
    let (hits, misses, ghost) = hour_of(cache, 1.0);
    let ghost_hits = Some(ghost.map(|hits| hits as u64));
    let nanos = |fills: f64| (fills * cache.load.cost * 1e9) as u64;
    let window = Activity {
        secs: 3600,
        capacity_bytes: cache.capacity,
        hits: hits as u64,
        misses: misses as u64,
        churn: Churn {
            ghost_hits,
            fills: misses as u64,
            fill_nanos: nanos(misses),
            prewarmed: cache.prewarmed,
            ..Churn::default()
        },
    };
    Row {
        entry: Entry::cache(cache.key.1, Scope::Process, SizeSource::Default),
        usage: Usage {
            capacity_bytes: cache.capacity,
            used_bytes: cache.capacity.min(cache.load.working_set),
            entries: Some(1000),
            hits: Some(hits as u64),
            misses: Some(misses as u64),
            churn: Some(Churn {
                ghost_hits,
                fills: 1,
                fill_nanos: nanos(1.0),
                prewarmed: cache.prewarmed,
                ..Churn::default()
            }),
        },
        window: Some(window),
        demand: None,
        configured_bytes: cache.configured,
        planned: false,
        least_bytes: 0,
    }
}

/// The plan `rows` fit to `total`.
fn sim_plan(rows: &[Row], total: u64) -> grant::Plan {
    plan(&Inputs::of_rows(
        total,
        1,
        Baseline::Estimated,
        rows,
        |_| None,
        false,
    ))
}

/// The least total in 64 MiB steps at which the plan grants every cache of
/// `caches` its configured size: grants as the caches stand, nothing over.
fn fitting_total(caches: &[SimCache]) -> u64 {
    let rows: Vec<Row> = caches.iter().map(sim_row).collect();
    let mut total = caches.iter().map(|c| c.configured).sum::<u64>();
    loop {
        let plan = sim_plan(&rows, total);
        if rows
            .iter()
            .all(|row| plan.grant(&row.entry).unwrap().bytes >= row.configured_bytes)
        {
            return total;
        }
        total += 64 * MIB;
    }
}

/// One re-plan tick as `tick` runs it, over simulated caches, in `apply`
/// mode with no valve: [`real_tick_with`].
fn real_tick(caches: &RefCell<Vec<SimCache>>, total: u64, ledger: &mut Ledger) -> Proposal {
    real_tick_with(
        caches,
        total,
        ledger,
        ElasticMode::Apply,
        &mut HashMap::new(),
        false,
    )
}

/// One re-plan tick as `tick` runs it, over simulated caches: fit the plan
/// to their configured sizes, keep the ledger as `mode` says
/// (`keep_ledger`), hold each cache to its grant plus its offset under the
/// valve's ceilings `caps` through `tick_target`, a restore step first when
/// `restore` (the process 10 minutes under 90 %), then weigh the capacities
/// that left and apply the moves (`run_elastic`), booked in the ledger,
/// while no ceiling holds. After it, every cache is at or above its floor
/// and, in `apply`, the ledger holds its invariant.
fn real_tick_with(
    caches: &RefCell<Vec<SimCache>>,
    total: u64,
    ledger: &mut Ledger,
    mode: ElasticMode,
    caps: &mut HashMap<Key, Ceiling>,
    restore: bool,
) -> Proposal {
    real_tick_ruled(caches, total, ledger, mode, caps, restore, (true, true))
}

/// [`real_tick_with`] under the tick's `(may_borrow, may_return)`
/// ([`decide::borrowing`]).
fn real_tick_ruled(
    caches: &RefCell<Vec<SimCache>>,
    total: u64,
    ledger: &mut Ledger,
    mode: ElasticMode,
    caps: &mut HashMap<Key, Ceiling>,
    restore: bool,
    rules: (bool, bool),
) -> Proposal {
    let set = |id: &str, bytes: u64| {
        for cache in caches.borrow_mut().iter_mut() {
            if cache.key.1 == id {
                cache.capacity = bytes;
            }
        }
    };
    let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
    let plan = sim_plan(&rows, total);
    // As the tick: a cache with a loan before or after is held exactly.
    let mut moved: HashSet<Key> = ledger.keys().into_iter().collect();
    keep_ledger(ledger, mode, &rows, Some(&plan));
    moved.extend(ledger.keys());
    let valve_holds = !caps.is_empty();
    let grant_of = |key: &Key| {
        rows.iter()
            .find(|row| decide::key(&row.entry) == *key)
            .and_then(|row| plan.grant(&row.entry))
            .map(|g| g.bytes)
    };
    let floor_of_key = |key: &Key| {
        let row = rows.iter().find(|row| decide::key(&row.entry) == *key)?;
        Some(floor_of(&row.entry, plan.grant(&row.entry)?))
    };
    decide::floor_ceilings(caps, &floor_of_key);
    let mut lifted = if restore {
        decide::restore(caps, &grant_of).1
    } else {
        HashSet::new()
    };
    lifted.extend(moved);
    for row in &rows {
        let key = decide::key(&row.entry);
        let grant = plan.grant(&row.entry).unwrap();
        let target = elastic_grant(ledger, &key, &row.entry, grant);
        let floor = floor_of(&row.entry, grant);
        let cap = caps.get(&key).map(|c| c.bytes);
        let to = tick_target(
            row.usage.capacity_bytes,
            target,
            floor,
            cap,
            lifted.contains(&key),
            false,
        );
        if let Some(to) = to {
            set(row.entry.id, to);
        }
    }
    let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
    let resize = |entry: &Entry, bytes: u64| {
        set(entry.id, bytes);
        Some(bytes)
    };
    let cell = RefCell::new(std::mem::take(ledger));
    let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut cell.borrow_mut());
    let proposal = run_elastic(
        &rows,
        &plan,
        &with_ledger,
        mode,
        valve_holds,
        rules,
        &resize,
    );
    *ledger = cell.into_inner();
    for row in &rows {
        let floor = floor_of(&row.entry, plan.grant(&row.entry).unwrap());
        let capacity = capacity_of(caches, row.entry.id);
        assert!(
            capacity >= floor,
            "{} below its floor: {caches:?}",
            row.entry.id
        );
    }
    if mode == ElasticMode::Apply {
        let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
        ledger_holds(&rows, &plan, ledger).unwrap();
    } else {
        assert!(ledger.is_empty());
    }
    proposal
}

/// `id`'s grant in the plan fitted to `caches` at `total`.
fn grant_in(caches: &RefCell<Vec<SimCache>>, total: u64, id: &str) -> u64 {
    let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
    let plan = sim_plan(&rows, total);
    let row = rows.iter().find(|r| r.entry.id == id).unwrap();
    plan.grant(&row.entry).unwrap().bytes
}

fn capacity_of(caches: &RefCell<Vec<SimCache>>, id: &str) -> u64 {
    caches
        .borrow()
        .iter()
        .find(|c| c.key.1 == id)
        .unwrap()
        .capacity
}

/// #1314 acceptance, through the real tick: of two caches, one thrashing
/// and one idle, capacity moves to the thrashing one within N ticks and
/// back when the load swaps; the idle one never goes below its floor. The
/// re-plan tick fits the plan to their configured sizes every tick, and the
/// ledger keeps it from moving them back (#1314 review: without it, two
/// 81 MiB steps on a 1 GiB cache came undone at the next tick).
#[test]
fn through_the_real_tick_memory_moves_to_the_thrashing_cache_and_back() {
    let caches = RefCell::new(vec![
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        sim_cache("rendered_cache", 4096, 512, IDLE),
    ]);
    let total = fitting_total(&caches.borrow());
    let metatile = (Kind::Cache, "metatile_cache", Scope::Process);
    let rendered = (Kind::Cache, "rendered_cache", Scope::Process);
    let mut ledger = Ledger::default();
    let mut ticks = 0;
    while capacity_of(&caches, "metatile_cache") < 3 * GIB {
        let proposal = real_tick(&caches, total, &mut ledger);
        assert!(
            proposal
                .moves
                .iter()
                .any(|m| m.kind == MoveKind::Borrow && id(&m.borrower) == "metatile_cache"),
            "tick {ticks}: {caches:?}"
        );
        ticks += 1;
        assert!(ticks <= 40, "{caches:?}");
    }
    let held = capacity_of(&caches, "metatile_cache");
    let grants = (
        grant_in(&caches, total, "metatile_cache"),
        grant_in(&caches, total, "rendered_cache"),
    );
    // A cache with loans is held to its grant plus its offset exactly.
    assert_eq!(ledger.offset(&metatile), held as i64 - grants.0 as i64);
    // Its working set held, it borrows no more, and the re-plan keeps it.
    // The idle cache decays to what it holds plus 10 %, by at most half a
    // step and 5 % of its size a tick, and stays there.
    let used = GIB;
    let decayed = (used + used / 10).div_ceil(MIB) * MIB;
    for _ in 0..60 {
        let before = capacity_of(&caches, "rendered_cache");
        let proposal = real_tick(&caches, total, &mut ledger);
        assert!(proposal.moves.iter().all(|m| m.kind == MoveKind::Decay));
        assert_eq!(capacity_of(&caches, "metatile_cache"), held);
        let after = capacity_of(&caches, "rendered_cache");
        assert!(after >= decayed);
        assert!(before - after <= (proposal.step_bytes / 2).min(before / 20));
    }
    assert_eq!(capacity_of(&caches, "rendered_cache"), decayed);
    assert_eq!(real_tick(&caches, total, &mut ledger).moves, []);
    assert_eq!(ledger.offset(&rendered), decayed as i64 - grants.1 as i64);
    assert_eq!(
        ledger.pool_net(),
        held as i64 + decayed as i64 - (grants.0 + grants.1) as i64
    );

    // The load swaps: read again, the cache takes back what it lent the
    // pool, at the restore's pace (#1473 review), then, below its grant,
    // what the other borrowed, at an even ratio, ahead of any other move.
    for cache in caches.borrow_mut().iter_mut() {
        cache.load = if cache.key.1 == "metatile_cache" {
            IDLE
        } else {
            thrash(3)
        };
    }
    let mut kinds = Vec::new();
    while capacity_of(&caches, "rendered_cache") < 3 * GIB {
        let proposal = real_tick(&caches, total, &mut ledger);
        let m = proposal
            .moves
            .iter()
            .find(|m| id(&m.borrower) == "rendered_cache")
            .expect("it grows every tick");
        kinds.push((m.kind, id(&m.lender)));
        assert!(kinds.len() <= 40, "{caches:?}");
    }
    let reclaims = kinds
        .iter()
        .position(|kind| *kind == (MoveKind::Reclaim, "metatile_cache"))
        .expect("a reclaim");
    assert!(reclaims > 0, "{kinds:?}");
    assert!(kinds[..reclaims]
        .iter()
        .all(|kind| *kind == (MoveKind::Return, "pool")));
    assert!(kinds[reclaims..]
        .iter()
        .all(|kind| *kind == (MoveKind::Reclaim, "metatile_cache")));
    assert_eq!(ledger.lent_to_pool(&rendered), 0, "its decay came back");
    assert!(capacity_of(&caches, "metatile_cache") < held);
}

/// #1314 review: an elastic offset is what the re-plan tick holds a cache
/// to, so the next tick leaves the moved capacity alone; without the
/// offset it moves it back to the grant once 10 % and 64 MiB off.
#[test]
fn the_replan_tick_holds_a_cache_to_its_grant_plus_its_offset() {
    let step = elastic_step(8 * GIB);
    let grant = GIB;
    for sign in [1i64, -1] {
        let offset = sign * 3 * step as i64;
        let capacity = (grant as i64 + offset) as u64;
        assert_eq!(
            tick_target(capacity, grant, 128 * MIB, None, false, false),
            Some(grant),
            "without the offset, the tick reverts it"
        );
        let target = elastic_target(grant, offset, 128 * MIB, 8 * GIB);
        assert_eq!(target, capacity);
        assert_eq!(
            tick_target(capacity, target, 128 * MIB, None, false, false),
            None
        );
    }
    // Within the floor and the ceiling.
    assert_eq!(
        elastic_target(GIB, -(2 * GIB as i64), 128 * MIB, 8 * GIB),
        128 * MIB
    );
    assert_eq!(
        elastic_target(GIB, 9 * GIB as i64, 128 * MIB, 8 * GIB),
        8 * GIB
    );
}

/// #1314 acceptance: a one-pass scan evicts plenty and misses every
/// lookup, but no key comes back: no ghost hits, so no move.
#[test]
fn a_scan_with_evictions_but_no_ghost_hits_moves_nothing() {
    let scan = Load {
        working_set: 100 * GIB,
        scan: true,
        ..thrash(100)
    };
    let mut caches = vec![
        sim_cache("scan_cache", 1024, 128, scan),
        sim_cache("idle_cache", 4096, 512, IDLE),
    ];
    for _ in 0..50 {
        assert_eq!(sim_tick(&mut caches, 8 * GIB, &mut |_| 1.0), []);
    }
}

/// #1314 acceptance: ±5 % noise in ghost hits moves nothing over 50 ticks,
/// between two caches under the same load, and once memory has moved.
/// Between two busy caches of unlike loads, noise near the edge may tip a
/// move more the same way, never one back.
#[test]
fn noise_of_5_percent_in_ghost_hits_moves_nothing_over_50_ticks() {
    let mut even = vec![
        sim_cache("a_cache", 2048, 128, thrash(3)),
        sim_cache("b_cache", 2048, 128, thrash(3)),
    ];
    let pool = held_by(&even);
    let mut wobble = noise(1314);
    for _ in 0..50 {
        assert_eq!(sim_tick(&mut even, pool, &mut wobble), []);
    }
    let mut moved = vec![
        sim_cache("a_cache", 1024, 128, thrash(3)),
        sim_cache("b_cache", 4096, 512, IDLE),
    ];
    let pool = held_by(&moved);
    for _ in 0..40 {
        sim_tick(&mut moved, pool, &mut |_| 1.0);
    }
    let settled: Vec<u64> = moved.iter().map(|c| c.capacity).collect();
    for _ in 0..50 {
        assert_eq!(sim_tick(&mut moved, pool, &mut wobble), [], "{moved:?}");
    }
    assert_eq!(
        moved.iter().map(|c| c.capacity).collect::<Vec<_>>(),
        settled
    );
    let mut busy = vec![
        sim_cache("a_cache", 1024, 128, thrash(3)),
        sim_cache(
            "b_cache",
            4096,
            512,
            Load {
                lookups: 50_000.0,
                ..thrash(6)
            },
        ),
    ];
    let pool = held_by(&busy);
    let mut directions = BTreeSet::new();
    for _ in 0..100 {
        for m in sim_tick(&mut busy, pool, &mut wobble) {
            directions.insert((id(&m.borrower), id(&m.lender)));
        }
    }
    assert!(
        !directions.contains(&("b_cache", "a_cache")),
        "a move back: {directions:?}"
    );
}

/// #1314 review, through the real tick: a busy cache that holds its
/// working set has no ghost hits, but what its bytes serve keeps it from
/// lending while an idle cache has memory to give, and the busy cache short
/// of memory borrows from the idle one. Before, the full cache weighed 0
/// as a lender, as the idle one did, and the key decided: it lent and
/// borrowed back in turn.
#[test]
fn a_full_busy_cache_does_not_lend_while_an_idle_one_can() {
    let caches = RefCell::new(vec![
        // Holds its 3 GiB working set; sorts before the idle cache.
        sim_cache("metatile_cache", 3072, 128, thrash(3)),
        // Short of its 3 GiB working set.
        sim_cache("rendered_cache", 1024, 128, thrash(3)),
        sim_cache("vector_tile_cache", 4096, 512, IDLE),
    ]);
    let total = fitting_total(&caches.borrow());
    let mut ledger = Ledger::default();
    let mut idle_lent = 0;
    for _ in 0..40 {
        for m in real_tick(&caches, total, &mut ledger).moves {
            assert_ne!(id(&m.lender), "metatile_cache", "{caches:?}");
            assert_ne!(id(&m.borrower), "metatile_cache", "{caches:?}");
            idle_lent += usize::from(id(&m.lender) == "vector_tile_cache");
        }
    }
    assert!(idle_lent > 0);
    assert!(
        capacity_of(&caches, "rendered_cache") >= 3 * GIB,
        "{caches:?}"
    );
    assert_eq!(capacity_of(&caches, "metatile_cache"), 3 * GIB);
}

/// #1314 review: a cache whose misses come back only past half its size
/// more (an animation loop just over it: an LRU cliff) borrows, its value
/// from the depth its ghost hits are at, gated there. Before, only the first
/// depth was read, which had none.
#[test]
fn a_cache_past_an_lru_cliff_borrows_by_its_best_depth() {
    // A loop over 1.8 × the cache: an LRU cache hits nothing, so every key
    // misses; a cache 2 × larger holds the loop, 1.5 × does not.
    let loop_cache = Elastic {
        key: (Kind::Cache, "metatile_cache", Scope::Process),
        capacity: 1024 * MIB,
        floor: 128 * MIB,
        ceiling: 8 * GIB,
        value: 0.0,
        ghost_hits: 0,
        depth: 0,
        borrows: true,
        lend_value: 0.0,
        lends: true,
        need: None,
        grant: 1024 * MIB,
        offset: 0,
        used: 1024 * MIB,
        idle: false,
        decays: true,
        pool_lent: 0,
    };
    let (value, depth) = best_depth(
        1024 * MIB,
        [0.0, 100_000.0, 100_000.0],
        0.003,
        ds_cache::plan::GHOST_DEPTH_FACTORS,
    );
    assert_eq!(depth, 1, "the cliff at 2 ×");
    assert!(value > 0.0);
    let looping = Elastic {
        value,
        depth,
        ghost_hits: 100_000,
        ..loop_cache.clone()
    };
    let idle = Elastic {
        key: (Kind::Cache, "rendered_cache", Scope::Process),
        capacity: 4 * GIB,
        floor: 512 * MIB,
        ..loop_cache.clone()
    };
    let idle = Elastic {
        grant: 4 * GIB,
        ..idle
    };
    let moves = elastic(&[looping.clone(), idle.clone()], 8 * GIB, 0, BORROW);
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].depth, 1);
    // Gated at that depth: under 10 ghost hits there, no borrowing.
    let rare = Elastic {
        ghost_hits: ELASTIC_MIN_GHOST_HITS - 1,
        ..looping
    };
    assert_eq!(elastic(&[rare, idle], 8 * GIB, 0, BORROW), []);
    // Equal values at two depths: the smaller step.
    assert_eq!(
        best_depth(GIB, [50.0, 100.0, 200.0], 1.0, [0.5, 1.0, 2.0]).1,
        0
    );
}

/// #1314 review: a move never lifts the expected use the plan fitted past
/// its pool. Lending capacity a working-set cache does not use to a cache
/// that fills all it gets would, and the pressure valve would take it back
/// ten minutes later, again and again; it moves what the headroom holds.
#[test]
fn a_move_keeps_the_expected_use_within_the_pool() {
    let pool = 3 * GIB;
    // A working-set cache of 4 GiB that holds 1 GiB: lending its unused
    // capacity lowers its expected use by nothing.
    let unused = SimCache {
        need: Some(GIB),
        ..sim_cache("grid_cache", 4096, 512, IDLE)
    };
    let expected = |caches: &[SimCache]| -> u64 {
        caches
            .iter()
            .map(|c| weigh(c, 1.0).expected_at(c.capacity))
            .sum()
    };
    // 1 GiB + 1 GiB expected: 1 GiB of headroom, which the borrower fills
    // and no more, though its working set is 3 GiB.
    let mut caches = vec![
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        unused.clone(),
    ];
    let mut moved = 0;
    for _ in 0..200 {
        moved += sim_tick(&mut caches, pool, &mut |_| 1.0).len();
        assert!(expected(&caches) <= pool, "{caches:?}");
    }
    assert!(moved > 0);
    assert_eq!(caches[0].capacity, 2 * GIB, "the headroom, filled");
    // At the pool already: no move, though the lender is worth nothing.
    let mut full = vec![sim_cache("metatile_cache", 2048, 128, thrash(8)), unused];
    assert_eq!(expected(&full), pool);
    assert_eq!(sim_tick(&mut full, pool, &mut |_| 1.0), []);
    // A lender whose expected use drops with what it gives lends there.
    let mut traffic = vec![
        sim_cache("metatile_cache", 2048, 128, thrash(8)),
        sim_cache("rendered_cache", 1024, 128, IDLE),
    ];
    assert_eq!(expected(&traffic), pool);
    assert_eq!(sim_tick(&mut traffic, pool, &mut |_| 1.0).len(), 1);
}

fn elastic_cache(id: &'static str, capacity_mib: u64, value: f64, ghost_hits: u64) -> Elastic {
    Elastic {
        key: (Kind::Cache, id, Scope::Process),
        capacity: capacity_mib * MIB,
        floor: 64 * MIB,
        ceiling: 8 * capacity_mib * MIB,
        value,
        ghost_hits,
        depth: 0,
        borrows: true,
        lend_value: value,
        lends: true,
        need: None,
        grant: capacity_mib * MIB,
        offset: 0,
        used: capacity_mib * MIB,
        idle: false,
        decays: true,
        pool_lent: 0,
    }
}

/// A move between caches needs a borrower worth twice its lender, with
/// enough ghost hits and fills in its window; it is the step, at most a
/// tenth of either cache, within the lender's floor and the borrower's
/// ceiling, in whole MiB; at most four a tick, each cache in one; a cache
/// that does not lend never does. No headroom: the pool lends nothing.
#[test]
fn a_move_needs_twice_the_value_and_stays_within_its_bounds() {
    let pool = 8 * GIB;
    let busy = |value| elastic_cache("busy_cache", 2048, value, 100);
    let quiet = elastic_cache("quiet_cache", 4096, 1.0, 100);
    assert_eq!(
        elastic(&[busy(1.99), quiet.clone()], pool, 0, BORROW),
        [],
        "under twice"
    );
    let moves = elastic(&[busy(2.0), quiet.clone()], pool, 0, BORROW);
    assert_eq!(moves.len(), 1);
    assert_eq!(
        (id(&moves[0].borrower), id(&moves[0].lender)),
        ("busy_cache", "quiet_cache")
    );
    assert_eq!(moves[0].bytes, 81 * MIB, "the step");
    assert_eq!((moves[0].borrower_value, moves[0].lender_value), (2.0, 1.0));

    let few = Elastic {
        ghost_hits: ELASTIC_MIN_GHOST_HITS - 1,
        ..busy(100.0)
    };
    assert_eq!(
        elastic(&[few, quiet.clone()], pool, 0, BORROW),
        [],
        "too few ghost hits"
    );
    let untimed = Elastic {
        borrows: false,
        ..busy(100.0)
    };
    assert_eq!(
        elastic(&[untimed, quiet.clone()], pool, 0, BORROW),
        [],
        "no fill timed"
    );
    let spared = Elastic {
        lends: false,
        lend_value: 0.0,
        ..quiet.clone()
    };
    assert_eq!(
        elastic(&[busy(10.0), spared], pool, 0, BORROW),
        [],
        "it never lends"
    );

    // A tenth of the smaller cache, its floor, its ceiling.
    let small = elastic_cache("small_cache", 300, 0.0, 0);
    assert_eq!(
        elastic(&[busy(10.0), small.clone()], pool, 0, BORROW)[0].bytes,
        30 * MIB
    );
    let at_floor = Elastic {
        floor: 290 * MIB,
        ..small.clone()
    };
    assert_eq!(
        elastic(&[busy(10.0), at_floor], pool, 0, BORROW)[0].bytes,
        10 * MIB
    );
    let near_ceiling = Elastic {
        ceiling: 2048 * MIB + 5 * MIB + 1,
        ..busy(10.0)
    };
    assert_eq!(
        elastic(&[near_ceiling, quiet.clone()], pool, 0, BORROW)[0].bytes,
        5 * MIB
    );
    let full = Elastic {
        ceiling: 2048 * MIB,
        ..busy(10.0)
    };
    assert_eq!(
        elastic(&[full, quiet.clone()], pool, 0, BORROW),
        [],
        "at its ceiling"
    );
    let tiny = elastic_cache("tiny_cache", 9, 10.0, 100);
    assert_eq!(
        elastic(&[tiny, quiet], pool, 0, BORROW),
        [],
        "a tenth of it is under 1 MiB"
    );

    // Six borrowers and six lenders: four moves, the best borrowers first,
    // each from the cheapest lender left.
    let mut caches: Vec<Elastic> = ["c1", "c2", "c3", "c4", "c5", "c6"]
        .iter()
        .enumerate()
        .map(|(i, id)| elastic_cache(id, 4096, 100.0 + i as f64, 100))
        .collect();
    caches.extend(
        ["l1", "l2", "l3", "l4", "l5", "l6"]
            .iter()
            .enumerate()
            .map(|(i, id)| elastic_cache(id, 4096, i as f64, 0)),
    );
    let pairs: Vec<(&str, &str)> = elastic(&caches, pool, 0, BORROW)
        .iter()
        .map(|m| (id(&m.borrower), id(&m.lender)))
        .collect();
    assert_eq!(
        pairs,
        [("c6", "l1"), ("c5", "l2"), ("c4", "l3"), ("c3", "l4")]
    );
}

/// The caches the tick weighs, from its rows and plan: those the plan
/// sizes, with a ring and a timed fill. A borrower's value is its best
/// depth's, from the window's ghost hits and miss cost, and only a window
/// that timed fills borrows; as a lender it is priced by what its bytes
/// serve too, and without a fill in the window by its mean fill since it
/// was built. No lending from a cache filled ahead of reads.
#[test]
fn the_tick_weighs_the_caches_the_plan_sizes_with_a_ring_and_a_timed_fill() {
    let hour = |ghost_hits: Option<[u64; 3]>, fills: u64| Activity {
        secs: 3600,
        capacity_bytes: GIB,
        hits: 900,
        misses: 100,
        churn: Churn {
            ghost_hits,
            fills,
            fill_nanos: fills * 2_000_000,
            ..Churn::default()
        },
    };
    let row = |id: &'static str, scope: Scope, source: SizeSource, window, ever: u64| Row {
        entry: Entry::cache(id, scope, source),
        usage: Usage {
            capacity_bytes: GIB,
            used_bytes: GIB / 2,
            entries: Some(10),
            hits: Some(900),
            misses: Some(100),
            churn: Some(Churn {
                fills: ever,
                fill_nanos: ever * 4_000_000,
                ..Churn::default()
            }),
        },
        window,
        demand: None,
        configured_bytes: GIB,
        planned: false,
        least_bytes: 32 * MIB,
    };
    let ring = Some([20, 30, 40]);
    let process = Scope::Process;
    let mut rows = vec![
        row(
            "metatile_cache",
            process.clone(),
            SizeSource::Default,
            Some(hour(ring, 50)),
            500,
        ),
        row(
            "rendered_cache",
            process.clone(),
            SizeSource::Config("[memory.caches] rendered"),
            Some(hour(ring, 50)),
            500,
        ),
        row(
            "vector_tile_cache",
            process.clone(),
            SizeSource::Default,
            Some(hour(None, 50)),
            500,
        ),
        row(
            "tiles3d_content_cache",
            process.clone(),
            SizeSource::Default,
            Some(hour(ring, 50)),
            500,
        ),
        row(
            "geotiff_decoded_chunk_cache",
            Scope::Engine("geotiff"),
            SizeSource::Default,
            Some(hour(ring, 0)),
            500,
        ),
        row(
            "odim_composite_cache",
            Scope::Engine("odim"),
            SizeSource::Default,
            Some(hour(ring, 0)),
            0,
        ),
        row(
            "satellite_frame_cache",
            Scope::Engine("satellite"),
            SizeSource::Default,
            Some(hour(ring, 50)),
            500,
        ),
        row(
            "pvol_voxel_grid_cache",
            Scope::Engine("odim-volume"),
            SizeSource::Default,
            None,
            500,
        ),
    ];
    rows.push(Row {
        usage: Usage {
            capacity_bytes: 0,
            ..rows[0].usage
        },
        ..row(
            "lightning_strike_cache",
            Scope::Engine("postgis"),
            SizeSource::Default,
            Some(hour(ring, 50)),
            500,
        )
    });
    let inputs = Inputs::of_rows(
        32 * GIB,
        4,
        Baseline::Estimated,
        &rows,
        |row| (row.entry.id == "tiles3d_content_cache").then(|| "no 3dtiles collection".into()),
        false,
    );
    let plan = plan(&inputs);
    let weighed = elastic_inputs(&rows, &plan, &Ledger::default());
    let ids: Vec<&str> = weighed.iter().map(|c| c.key.1).collect();
    assert_eq!(
        ids,
        [
            "metatile_cache",
            "geotiff_decoded_chunk_cache",
            "satellite_frame_cache"
        ],
        "pinned, without a ring, unreachable, never timed, unsampled and disabled ones drop"
    );
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-12 * b;
    let metatile = &weighed[0];
    // Its best depth: 20 ghost hits over the 512 MiB of 0.5 × against 30
    // over 1 GiB and 40 over 2 GiB, at 2 ms.
    assert_eq!((metatile.depth, metatile.ghost_hits), (0, 20));
    assert!(
        close(metatile.value, 20.0 * 0.002 / (512.0 * MIB as f64)),
        "{metatile:?}"
    );
    // As a lender: 900 hits an hour at 2 ms over its 1 GiB is more.
    assert!(close(metatile.lend_value, 900.0 * 0.002 / GIB as f64));
    assert!(metatile.borrows && metatile.lends);
    let grant = plan.grant(&rows[0].entry).unwrap();
    assert_eq!(metatile.ceiling, grant.ceiling_bytes);
    assert_eq!(metatile.floor, grant.floor_bytes.max(32 * MIB));
    // Ghost hits and no fill in the window: refills failing or untimed. It
    // does not borrow; it lends at its mean fill since it was built, 4 ms.
    let decoded = &weighed[1];
    assert!(!decoded.borrows);
    assert!(close(decoded.lend_value, 900.0 * 0.004 / GIB as f64));
    assert!(!weighed[2].lends, "a demand cache, filled ahead of reads");
    assert!(!weighed[2].decays);
    assert!(metatile.decays && !metatile.idle);
    assert_eq!((metatile.grant, metatile.offset), (grant.bytes, 0));
    assert_eq!(metatile.used, GIB / 2);
    // #1314 C: neither a cache filled ahead of its reads nor one an engine
    // publishes a floor or a fill ahead for decays while idle, and the
    // offset is the ledger's.
    let quiet = |row: &Row| Row {
        window: row.window.as_ref().map(|w| Activity {
            hits: 0,
            misses: 0,
            ..*w
        }),
        ..row.clone()
    };
    let mut prewarmed = quiet(&rows[0]);
    prewarmed.usage.churn.as_mut().unwrap().prewarmed = true;
    let mut ahead = quiet(&rows[0]);
    ahead.demand = Some(Demand {
        ahead: true,
        ..demand(GIB, MIB)
    });
    let mut floored = quiet(&rows[0]);
    floored.demand = Some(Demand {
        floor: true,
        ..demand(GIB, MIB)
    });
    let mut ledger = Ledger::default();
    let party = Party {
        key: decide::key(&rows[0].entry),
        source: SizeSource::Default,
        configured: GIB,
    };
    ledger.book(Some(party), None, 64 * MIB);
    for (row, decays) in [
        (quiet(&rows[0]), true),
        (prewarmed, false),
        (ahead, false),
        (floored, false),
    ] {
        let rows = [row];
        let plan = sim_plan(&rows, 32 * GIB);
        let weighed = elastic_inputs(&rows, &plan, &ledger);
        assert!(weighed[0].idle);
        assert_eq!(weighed[0].decays, decays, "{:?}", rows[0]);
        assert_eq!(weighed[0].offset, 64 * MIB as i64);
    }
}

/// A loan's party: `id`'s process cache, configured at `configured`.
fn party(id: &'static str, configured: u64) -> Party {
    Party {
        key: (Kind::Cache, id, Scope::Process),
        source: SizeSource::Default,
        configured,
    }
}

/// The ledger (#1314): a reload that pins, re-bounds or resizes a cache
/// ends its loans, and so does one that rebuilds or removes its collection;
/// the counterparts shed exactly those bytes, so the offsets still sum to
/// what the pool lent. A move back repays before it lends.
#[test]
fn the_ledger_ends_the_loans_a_reload_supersedes_on_both_sides() {
    let key = |id| (Kind::Cache, id, Scope::Process);
    let tile = |id: &str| Party {
        key: (
            Kind::Cache,
            "tile_cache",
            Scope::Collection {
                engine: "geotiff",
                id: id.into(),
            },
        ),
        source: SizeSource::Default,
        configured: GIB,
    };
    let mut ledger = Ledger::default();
    ledger.book(
        Some(party("metatile_cache", GIB)),
        Some(party("rendered_cache", GIB)),
        512 * MIB,
    );
    ledger.book(Some(party("metatile_cache", GIB)), None, 256 * MIB);
    ledger.book(
        Some(party("vector_tile_cache", GIB)),
        Some(party("metatile_cache", GIB)),
        64 * MIB,
    );
    ledger.book(Some(tile("radar")), None, 64 * MIB);
    ledger.balanced().unwrap();
    assert_eq!(
        ledger.offset(&key("metatile_cache")),
        (512 + 256 - 64) * MIB as i64
    );

    // A move back repays the loan it reverses before it lends.
    let mut back = ledger.clone();
    back.book(
        Some(party("rendered_cache", GIB)),
        Some(party("metatile_cache", GIB)),
        600 * MIB,
    );
    assert_eq!(back.offset(&key("rendered_cache")), 88 * MIB as i64);
    assert_eq!(
        back.offset(&key("metatile_cache")),
        (256 - 64 - 88) * MIB as i64
    );
    back.balanced().unwrap();
    assert_eq!(back.lent(&key("rendered_cache")), 0, "repaid");
    assert_eq!(back.borrowed(&key("rendered_cache")), 88 * MIB);

    let caches = [
        sim_cache("metatile_cache", 1024, 128, IDLE),
        SimCache {
            configured: 2 * GIB,
            ..sim_cache("rendered_cache", 1024, 128, IDLE)
        },
    ];
    let mut rows: Vec<Row> = caches.iter().map(sim_row).collect();
    let mut pinned = sim_row(&sim_cache("vector_tile_cache", 1024, 128, IDLE));
    pinned.entry.source = SizeSource::Config("[memory.caches] vector_tile");
    rows.push(pinned);
    reconcile_ledger(&mut ledger, &rows);
    // Resized, pinned and gone ones end, and so do their counterparts' sides:
    // the metatile cache keeps what the pool lent it.
    assert_eq!(ledger.keys(), [key("metatile_cache")]);
    assert_eq!(ledger.offset(&key("metatile_cache")), 256 * MIB as i64);
    ledger.balanced().unwrap();

    ledger.book(Some(tile("radar")), Some(tile("opera")), 64 * MIB);
    ledger.book(Some(tile("opera")), None, 32 * MIB);
    ledger.forget(&["radar"]);
    assert_eq!(ledger.offset(&tile("radar").key), 0, "rebuilt");
    assert_eq!(
        ledger.offset(&tile("opera").key),
        32 * MIB as i64,
        "its loan to radar ended"
    );
    ledger.balanced().unwrap();
}

/// #1314 review probes: the ledger stays balanced when a reload resizes
/// the lender or a tighter total bounds it: what the borrower holds falls
/// with what the lender gave, the offsets net to nothing, and the loans
/// keep the expected use within the pool. A cache over its ceiling sheds
/// what it borrowed, the pool's first.
#[test]
fn the_ledger_stays_balanced_through_reloads_and_tighter_plans() {
    let key = |id| (Kind::Cache, id, Scope::Process);
    let caches = vec![
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        sim_cache("rendered_cache", 4096, 512, IDLE),
    ];
    let balanced = || {
        let mut ledger = Ledger::default();
        ledger.book(
            Some(party("metatile_cache", GIB)),
            Some(party("rendered_cache", 4 * GIB)),
            512 * MIB,
        );
        ledger
    };
    let net = |ledger: &Ledger| ledger.keys().iter().map(|k| ledger.offset(k)).sum::<i64>();

    // A reload gives the lender another configured size.
    let mut resized = caches.clone();
    resized[1].configured = 2 * GIB;
    let rows: Vec<Row> = resized.iter().map(sim_row).collect();
    let mut ledger = balanced();
    keep_ledger(
        &mut ledger,
        ElasticMode::Apply,
        &rows,
        Some(&sim_plan(&rows, 8 * GIB)),
    );
    assert_eq!(net(&ledger), 0);
    assert!(
        ledger.is_empty(),
        "the borrower's side ends with the lender's"
    );

    // A tighter total: 2 GiB.
    let rows: Vec<Row> = caches.iter().map(sim_row).collect();
    let plan = sim_plan(&rows, 2 * GIB);
    let mut ledger = balanced();
    // Bound directly: `keep_ledger` would clear a ledger left broken.
    bound_ledger(&mut ledger, &rows, &plan);
    assert_eq!(net(&ledger), 0);
    ledger_holds(&rows, &plan, &ledger).unwrap();
    for row in &rows {
        let grant = plan.grant(&row.entry).unwrap();
        let target = elastic_grant(&ledger, &decide::key(&row.entry), &row.entry, grant);
        assert!(target >= floor_of(&row.entry, grant) && target <= grant.ceiling_bytes);
    }

    // Over its ceiling a cache sheds what it borrowed, the pool's loan
    // first; under its floor what it lent.
    let mut ledger = Ledger::default();
    ledger.book(Some(party("metatile_cache", GIB)), None, GIB);
    ledger.book(
        Some(party("metatile_cache", GIB)),
        Some(party("rendered_cache", GIB)),
        512 * MIB,
    );
    let bounds = |key: &Key| match key.1 {
        "metatile_cache" => Some((GIB, 128 * MIB, 2 * GIB)),
        _ => Some((GIB, 768 * MIB, 4 * GIB)),
    };
    ledger.bound(&bounds, &|_| 1.0);
    assert_eq!(ledger.loans().len(), 2);
    assert_eq!(
        ledger.lent(&key("rendered_cache")),
        256 * MIB,
        "to its floor"
    );
    assert_eq!(ledger.offset(&key("metatile_cache")), 768 * MIB as i64);
    assert_eq!(
        ledger.pool_net(),
        512 * MIB as i64,
        "the pool's loan shed first"
    );
    ledger.balanced().unwrap();

    // A borrower lifting the expected use past the pool sheds its loans,
    // from the lowest-valued borrower up.
    let rows: Vec<Row> = caches.iter().map(sim_row).collect();
    let plan = sim_plan(&rows, fitting_total(&caches));
    let mut ledger = Ledger::default();
    ledger.book(Some(party("metatile_cache", GIB)), None, 4 * GIB);
    bound_ledger(&mut ledger, &rows, &plan);
    assert!(expected_use(&rows, &plan, &ledger) <= plan.pool_bytes);
    ledger_holds(&rows, &plan, &ledger).unwrap();
}

/// While the pressure valve holds, elastic memory weighs nothing; else it
/// proposes the moves the decision makes. In report mode they resize
/// nothing and leave the ledger empty; the gauges show the current moves,
/// and the counter and the log only new ones. Off weighs nothing and shows
/// nothing. Listed in `/admin/cache-plan` with every cache weighed and the
/// ledger's loans and offsets.
#[test]
fn proposals_wait_for_the_valve_and_report_mode_counts_only_new_moves() {
    let caches = vec![
        elastic_cache("metatile_cache", 4096, 1e-9, 100),
        Elastic {
            key: (
                Kind::Cache,
                "tile_cache",
                Scope::Collection {
                    engine: "geotiff",
                    id: "radar".into(),
                },
            ),
            ..elastic_cache("tile_cache", 1024, 0.0, 0)
        },
    ];
    let moves = elastic(&caches, 8 * GIB, 0, BORROW);
    let proposal = Proposal {
        at: chrono::Utc::now(),
        mode: ElasticMode::Report,
        held: None,
        may_borrow: true,
        may_return: true,
        step_bytes: elastic_step(8 * GIB),
        headroom_bytes: GIB,
        caches: caches.clone(),
        moves: moves.clone(),
    };
    let block = elastic_block(&proposal);
    assert_eq!(
        block,
        format!(
            "Elastic memory, report only, nothing resized (#1314): 2 caches weighed, 1 move a \
             tick would make, in steps of at most 81 MiB\n  tile_cache radar → metatile_cache: \
             81 MiB (borrow); a MiB more saves {:.3} ms of fills an hour there, on average up \
             to 1.5 × its size more, against 0.000 a MiB serves where it comes from",
            crate::cache_plan::ms_per_mib(1e-9)
        )
    );
    let none = Proposal {
        moves: Vec::new(),
        ..proposal.clone()
    };
    assert!(
        elastic_block(&none).ends_with(", no move: no cache's MiB more is worth 2 times another's")
    );
    let applied = Proposal {
        mode: ElasticMode::Apply,
        moves: vec![
            Move {
                kind: MoveKind::Reclaim,
                ..moves[0].clone()
            },
            Move {
                kind: MoveKind::Decay,
                borrower: None,
                ..moves[0].clone()
            },
        ],
        ..proposal.clone()
    };
    let block = elastic_block(&applied);
    assert!(
        block.starts_with("Elastic memory, applied (#1314)"),
        "{block}"
    );
    assert!(block.contains("2 moves made"), "{block}");
    assert!(
        block.contains("\n  tile_cache radar → metatile_cache: 81 MiB (reclaim); back to its plan"),
        "{block}"
    );
    assert!(
        block.contains("\n  tile_cache radar → the pool: 81 MiB (decay); idle"),
        "{block}"
    );

    let mut ledger = Ledger::default();
    ledger.book(Some(party("metatile_cache", GIB)), None, 64 * MIB);
    let document = elastic_json(&proposal, &ledger);
    assert_eq!(document["mode"], "report");
    assert_eq!(document["held"], Value::Null);
    assert_eq!(document["may_borrow"], true);
    assert_eq!(document["step_bytes"], 81 * MIB);
    assert_eq!(document["headroom_bytes"], GIB);
    assert_eq!(document["moves"][0]["kind"], "borrow");
    assert_eq!(
        document["moves"][0]["lender"],
        json!({"id": "tile_cache", "collection": "radar"})
    );
    assert_eq!(document["moves"][0]["bytes"], 81 * MIB);
    assert_eq!(document["moves"][0]["borrower_depth"], "0.5");
    assert_eq!(document["caches"][1]["floor_bytes"], 64 * MIB);
    assert_eq!(document["caches"][1]["grant_bytes"], GIB);
    assert_eq!(document["caches"][1]["lends"], true);
    assert_eq!(
        document["ledger"],
        json!({
            "loans": [{
                "borrower": {"id": "metatile_cache", "collection": null},
                "lender": "pool",
                "bytes": 64 * MIB,
            }],
            "offsets": [{"id": "metatile_cache", "collection": null, "offset_bytes": 64 * MIB}],
        })
    );
    assert_eq!(
        elastic_json(&applied, &ledger)["moves"][1]["borrower"],
        "pool"
    );

    let counted = |cache: &str, role: &str| {
        ELASTIC_MOVES
            .with_label_values(&[cache, role, "report"])
            .get()
    };
    let before = (
        counted("metatile_cache", "borrow"),
        counted("tile_cache", "lend"),
    );
    record_elastic(proposal.clone());
    record_elastic(proposal.clone());
    assert_eq!(
        (
            counted("metatile_cache", "borrow"),
            counted("tile_cache", "lend")
        ),
        (before.0 + 1, before.1 + 1),
        "a move that recurs counts once"
    );
    assert_eq!(
        ELASTIC_PROPOSED
            .with_label_values(&["metatile_cache", "borrow"])
            .get(),
        81 * MIB as i64
    );

    // The valve holds: nothing weighed, nothing moved; report mode resizes
    // nothing and keeps no loan; off weighs nothing.
    let rows: Vec<Row> = [
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        sim_cache("rendered_cache", 4096, 512, IDLE),
    ]
    .iter()
    .map(sim_row)
    .collect();
    let plan = sim_plan(&rows, 32 * GIB);
    let ledger = RefCell::new(Ledger::default());
    let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut ledger.borrow_mut());
    let resize = |_: &Entry, _: u64| -> Option<u64> { panic!("report mode resized") };
    let run = |mode, valve_holds| {
        run_elastic(
            &rows,
            &plan,
            &with_ledger,
            mode,
            valve_holds,
            (true, true),
            &resize,
        )
    };
    let held = run(ElasticMode::Report, true);
    assert_eq!(held.held, Some(VALVE_HOLDS));
    assert!(held.moves.is_empty() && held.caches.is_empty());
    let reported = run(ElasticMode::Report, false);
    assert_eq!(reported.moves.len(), 1, "{reported:?}");
    assert!(ledger.borrow().is_empty());
    assert_eq!(elastic_json(&held, &ledger.borrow())["held"], VALVE_HOLDS);
    let off = run(ElasticMode::Off, false);
    assert_eq!((off.held, off.caches.len(), off.moves.len()), (None, 0, 0));
    assert_eq!(elastic_json(&off, &ledger.borrow())["mode"], "off");
}

/// #1314 C: the pool's free headroom lends first, worth nothing where it
/// is, so a borrower needs only to pass the gate; never past the headroom,
/// and not at all while borrowing is off (resident at or above 90 % of the
/// total). Without headroom, caches lend by the 2 × rule.
#[test]
fn the_pool_lends_its_headroom_first_and_never_past_it() {
    let pool = 8 * GIB;
    let busy = elastic_cache("busy_cache", 2048, 1.0, 100);
    let quiet = elastic_cache("quiet_cache", 4096, 1.0, 0);
    let moves = elastic(&[busy.clone(), quiet.clone()], pool, 100 * MIB, BORROW);
    assert_eq!(moves.len(), 1);
    assert_eq!(
        (moves[0].kind, id(&moves[0].borrower), id(&moves[0].lender)),
        (MoveKind::Borrow, "busy_cache", "pool")
    );
    assert_eq!((moves[0].bytes, moves[0].lender_value), (81 * MIB, 0.0));
    assert_eq!(
        elastic(&[busy.clone(), quiet.clone()], pool, 30 * MIB, BORROW)[0].bytes,
        30 * MIB,
        "what the headroom holds"
    );
    let no_borrowing = Rules {
        may_borrow: false,
        may_return: true,
        decays: false,
    };
    assert_eq!(
        elastic(&[busy.clone(), quiet.clone()], pool, GIB, no_borrowing),
        []
    );
    assert_eq!(elastic(&[busy, quiet], pool, 0, BORROW), [], "under twice");
}

/// #1314 C: a cache below its grant, its fair share, that passes the gate
/// takes back what a borrower holds, the lowest-valued borrower first, at
/// an even ratio, up to its grant and what the borrower borrowed; ahead of
/// every borrow, and while borrowing is off. Above its grant the 2 × rule
/// holds.
#[test]
fn a_cache_below_its_grant_reclaims_from_the_lowest_valued_borrower() {
    let pool = 8 * GIB;
    let reclaimer = Elastic {
        grant: 2 * GIB,
        ..elastic_cache("short_cache", 1024, 1.0, 100)
    };
    let donor = |id, lend_value, offset_mib: i64| Elastic {
        offset: offset_mib * MIB as i64,
        lend_value,
        value: 0.0,
        ghost_hits: 0,
        ..elastic_cache(id, 4096, 0.0, 0)
    };
    let caches = [
        reclaimer.clone(),
        donor("dear_cache", 0.5, 512),
        donor("cheap_cache", 0.2, 512),
    ];
    let no_borrowing = Rules {
        may_borrow: false,
        may_return: true,
        decays: false,
    };
    for rules in [BORROW, no_borrowing] {
        let moves = elastic(&caches, pool, 0, rules);
        assert_eq!(moves.len(), 1, "{moves:?}");
        assert_eq!(
            (moves[0].kind, id(&moves[0].borrower), id(&moves[0].lender)),
            (MoveKind::Reclaim, "short_cache", "cheap_cache")
        );
        assert_eq!(moves[0].bytes, 81 * MIB, "the step");
    }
    // An even ratio: worth what the borrower's bytes serve, not twice.
    let even = [
        Elastic {
            value: 0.5,
            ..reclaimer.clone()
        },
        donor("dear_cache", 0.5, 512),
    ];
    assert_eq!(elastic(&even, pool, 0, BORROW)[0].kind, MoveKind::Reclaim);
    let less = [
        Elastic {
            value: 0.49,
            ..reclaimer.clone()
        },
        donor("dear_cache", 0.5, 512),
    ];
    assert_eq!(elastic(&less, pool, 0, BORROW), []);
    // Up to its grant, and what the borrower borrowed.
    let near = [
        Elastic {
            capacity: 2 * GIB - 10 * MIB,
            ..reclaimer.clone()
        },
        donor("cheap_cache", 0.2, 512),
    ];
    assert_eq!(elastic(&near, pool, 0, BORROW)[0].bytes, 10 * MIB);
    let little = [reclaimer.clone(), donor("cheap_cache", 0.2, 5)];
    assert_eq!(elastic(&little, pool, 0, BORROW)[0].bytes, 5 * MIB);
    // At its grant, the 2 × rule: 0.9 is not twice 0.5.
    let at_grant = [
        Elastic {
            grant: GIB,
            value: 0.9,
            ..reclaimer.clone()
        },
        donor("dear_cache", 0.5, 512),
    ];
    assert_eq!(elastic(&at_grant, pool, 0, BORROW), []);
    // A lender that borrowed nothing is not reclaimed from.
    let lent_only = [reclaimer, donor("cheap_cache", 0.2, -512)];
    assert_eq!(elastic(&lent_only, pool, 0, no_borrowing), []);
}

/// #1314 C: an idle cache that decays and moved nothing this tick gives
/// the pool what it does not hold, by at most half the step and 5 % of its
/// capacity, down to the larger of its floor and what it holds plus 10 %;
/// only where moves apply.
#[test]
fn an_idle_cache_decays_by_half_a_step_toward_what_it_holds() {
    let pool = 8 * GIB;
    let decays = Rules {
        may_borrow: true,
        may_return: true,
        decays: true,
    };
    let idle = Elastic {
        idle: true,
        used: 512 * MIB,
        value: 0.0,
        ghost_hits: 0,
        ..elastic_cache("idle_cache", 4096, 0.0, 0)
    };
    let moves = elastic(std::slice::from_ref(&idle), pool, 0, decays);
    assert_eq!(moves.len(), 1);
    assert_eq!(
        (moves[0].kind, id(&moves[0].borrower), id(&moves[0].lender)),
        (MoveKind::Decay, "pool", "idle_cache")
    );
    assert_eq!(moves[0].bytes, 40 * MIB, "half of the 81 MiB step");
    assert_eq!(elastic(std::slice::from_ref(&idle), pool, 0, BORROW), []);
    let small = Elastic {
        capacity: 600 * MIB,
        used: 100 * MIB,
        ..idle.clone()
    };
    assert_eq!(
        elastic(&[small], pool, 0, decays)[0].bytes,
        30 * MIB,
        "5 % of it"
    );
    let near = Elastic {
        capacity: 570 * MIB,
        ..idle.clone()
    };
    assert_eq!(
        elastic(&[near], pool, 0, decays)[0].bytes,
        6 * MIB,
        "to what it holds plus 10 %, 564 MiB"
    );
    let floored = Elastic {
        floor: 4090 * MIB,
        ..idle.clone()
    };
    assert_eq!(elastic(&[floored], pool, 0, decays)[0].bytes, 6 * MIB);
    for spared in [
        Elastic {
            decays: false,
            pool_lent: 0,
            ..idle.clone()
        },
        Elastic {
            idle: false,
            ..idle.clone()
        },
        Elastic {
            capacity: 563 * MIB,
            ..idle.clone()
        },
    ] {
        assert_eq!(elastic(&[spared], pool, 0, decays), []);
    }
    // A cache that lent this tick does not decay too.
    let busy = elastic_cache("busy_cache", 2048, 10.0, 100);
    let moves = elastic(&[busy, idle], pool, 0, decays);
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].kind, MoveKind::Borrow);
}

/// #1314 C, through the real tick: idle caches decay to what they hold plus
/// 10 %, or their floor, and stay there, each a loan to the pool; a cache
/// filled ahead of its reads, such as the PVOL pixels that keep a quiet
/// deployment warm, never decays.
#[test]
fn through_the_real_tick_idle_caches_decay_and_prewarmed_ones_never_do() {
    let caches = RefCell::new(vec![
        sim_cache("rendered_cache", 4096, 512, IDLE),
        sim_cache(
            "vector_tile_cache",
            2048,
            256,
            Load {
                working_set: 128 * MIB,
                ..IDLE
            },
        ),
        SimCache {
            prewarmed: true,
            ..sim_cache("metatile_cache", 2048, 256, IDLE)
        },
    ]);
    let total = fitting_total(&caches.borrow());
    let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
    let plan = sim_plan(&rows, total);
    let floor = |id: &str| {
        let row = rows.iter().find(|r| r.entry.id == id).unwrap();
        floor_of(&row.entry, plan.grant(&row.entry).unwrap())
    };
    let rendered_to = (GIB + GIB / 10).div_ceil(MIB) * MIB;
    let vector_to = floor("vector_tile_cache").max(141 * MIB);
    assert!(
        vector_to > 141 * MIB,
        "the floor, above what it holds plus 10 %"
    );
    let mut ledger = Ledger::default();
    for _ in 0..80 {
        let before: Vec<u64> = caches.borrow().iter().map(|c| c.capacity).collect();
        let proposal = real_tick(&caches, total, &mut ledger);
        assert!(proposal.moves.iter().all(|m| m.kind == MoveKind::Decay));
        for (cache, before) in caches.borrow().iter().zip(before) {
            let step = (proposal.step_bytes / 2).min(before / 20);
            assert!(before - cache.capacity <= step, "{cache:?}");
        }
        assert_eq!(capacity_of(&caches, "metatile_cache"), 2 * GIB);
    }
    assert_eq!(capacity_of(&caches, "rendered_cache"), rendered_to);
    assert_eq!(capacity_of(&caches, "vector_tile_cache"), vector_to);
    assert_eq!(real_tick(&caches, total, &mut ledger).moves, []);
    let lent = 4 * GIB - rendered_to + 2 * GIB - vector_to;
    assert_eq!(ledger.pool_net(), -(lent as i64));
    assert!(ledger.loans().iter().all(|loan| loan.borrower.is_none()));
}

/// #1314 C: what a borrower could not take of what its lender gave goes
/// back to the lender; the loan and the move are what the borrower gained,
/// and a borrower gone or refusing everything leaves the lender as it was
/// and books nothing.
#[test]
fn a_borrower_short_of_the_move_gives_the_rest_back_to_its_lender() {
    let caches = RefCell::new(vec![
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        sim_cache("rendered_cache", 4096, 512, IDLE),
    ]);
    let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
    let template = Move {
        kind: MoveKind::Borrow,
        borrower: Some((Kind::Cache, "metatile_cache", Scope::Process)),
        lender: Some((Kind::Cache, "rendered_cache", Scope::Process)),
        bytes: 100 * MIB,
        borrower_value: 1.0,
        depth: 0,
        lender_value: 0.0,
    };
    for (most, gained) in [(Some(40 * MIB), 40 * MIB), (Some(0), 0), (None, 0)] {
        caches.borrow_mut()[0].capacity = GIB;
        caches.borrow_mut()[1].capacity = 4 * GIB;
        let resize = |entry: &Entry, bytes: u64| {
            let mut caches = caches.borrow_mut();
            let cache = caches.iter_mut().find(|c| c.key.1 == entry.id).unwrap();
            if entry.id == "metatile_cache" {
                let had = cache.capacity;
                cache.capacity = bytes.min(had + most?);
            } else {
                cache.capacity = bytes;
            }
            Some(cache.capacity)
        };
        let ledger = RefCell::new(Ledger::default());
        let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut ledger.borrow_mut());
        let mut m = template.clone();
        assert_eq!(
            apply_move(&mut m, &rows, &with_ledger, &resize),
            gained > 0,
            "{most:?}"
        );
        assert_eq!(capacity_of(&caches, "rendered_cache"), 4 * GIB - gained);
        let ledger = ledger.into_inner();
        assert_eq!(ledger.lent(&template.lender.clone().unwrap()), gained);
        if gained > 0 {
            assert_eq!(m.bytes, gained);
        }
    }
    // #1473 review: a lender whose regrow stops short keeps what it did not
    // get back as a loan to the pool, so the ledger holds what the sizes
    // did.
    caches.borrow_mut()[0].capacity = GIB;
    caches.borrow_mut()[1].capacity = 4 * GIB;
    let resize = |entry: &Entry, bytes: u64| {
        let mut caches = caches.borrow_mut();
        let cache = caches.iter_mut().find(|c| c.key.1 == entry.id).unwrap();
        cache.capacity = match entry.id {
            "metatile_cache" => bytes.min(cache.capacity + 40 * MIB),
            // Shrinks, but grows back 10 MiB at most.
            _ => bytes.min(cache.capacity + 10 * MIB),
        };
        Some(cache.capacity)
    };
    let ledger = RefCell::new(Ledger::default());
    let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut ledger.borrow_mut());
    let mut m = template.clone();
    assert!(apply_move(&mut m, &rows, &with_ledger, &resize));
    assert_eq!(m.bytes, 40 * MIB);
    assert_eq!(capacity_of(&caches, "rendered_cache"), 4 * GIB - 90 * MIB);
    let ledger = ledger.into_inner();
    let rendered = template.lender.clone().unwrap();
    assert_eq!(ledger.lent(&rendered), 90 * MIB, "what it gave and kept");
    assert_eq!(ledger.lent_to_pool(&rendered), 50 * MIB);
    ledger.balanced().unwrap();
}

/// #1314 C, through the real tick and the valve: under pressure, what
/// caches borrowed goes back first, the lowest-valued borrower first, its
/// lender staying where it is; when that covers the excess nothing else is
/// lowered. When it does not, the others are lowered to 90 % and lose
/// their loans, the restore climbs to the plan, and once every ceiling is
/// lifted the borrower borrows again by no more than a step a tick.
#[test]
fn under_pressure_borrowers_give_back_first_and_restores_climb_to_the_plan() {
    let metatile = (Kind::Cache, "metatile_cache", Scope::Process);
    let rendered = (Kind::Cache, "rendered_cache", Scope::Process);
    let start = || {
        RefCell::new(vec![
            sim_cache("metatile_cache", 1024, 128, thrash(3)),
            sim_cache("vector_tile_cache", 1024, 128, thrash(2)),
            sim_cache("rendered_cache", 4096, 512, IDLE),
        ])
    };
    let caches = start();
    let total = fitting_total(&caches.borrow());
    let vector = (Kind::Cache, "vector_tile_cache", Scope::Process);
    // Until both borrowed: the one lender lends to one borrower a tick.
    let borrow = |caches: &RefCell<Vec<SimCache>>| {
        let mut ledger = Ledger::default();
        for _ in 0..40 {
            if ledger.borrowed(&metatile) > 0 && ledger.borrowed(&vector) > 0 {
                break;
            }
            real_tick(caches, total, &mut ledger);
        }
        ledger
    };
    let ledger = borrow(&caches);
    let (borrowed, lent) = (ledger.borrowed(&metatile), ledger.lent(&rendered));
    assert!(borrowed > 0 && ledger.borrowed(&vector) > 0, "{ledger:?}");
    let relieve_by = |caches: &RefCell<Vec<SimCache>>, ledger: &mut Ledger, excess: u64| {
        let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
        let plan = sim_plan(&rows, total);
        let resize = |entry: &Entry, bytes: u64| {
            let mut caches = caches.borrow_mut();
            let cache = caches.iter_mut().find(|c| c.key.1 == entry.id).unwrap();
            cache.capacity = bytes;
            Some(bytes)
        };
        let cell = RefCell::new(std::mem::take(ledger));
        let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut cell.borrow_mut());
        let relief = relief(
            &rows,
            Some(&plan),
            &with_ledger,
            ElasticMode::Apply,
            excess,
            &resize,
        );
        *ledger = cell.into_inner();
        ledger_holds(&rows, &plan, ledger).unwrap();
        relief
    };

    // A small excess: the lowest-valued borrower gives back what it
    // borrowed, a byte of its serving less; nothing else moves.
    let mut small = ledger.clone();
    let before = capacity_of(&caches, "metatile_cache");
    let vector_before = capacity_of(&caches, "vector_tile_cache");
    let relief = relieve_by(&caches, &mut small, MIB);
    assert_eq!(relief.taken.len(), 1, "{relief:?}");
    assert_eq!(
        relief.taken[0].entry.id, "metatile_cache",
        "worth less a byte"
    );
    // #1473 review: only the excess, in whole MiB; it holds all it has.
    assert_eq!(relief.taken[0].to_bytes, before - MIB);
    assert!(relief.changes.is_empty() && relief.caps.is_empty());
    assert_eq!(small.borrowed(&metatile), borrowed - MIB);
    assert_eq!(capacity_of(&caches, "vector_tile_cache"), vector_before);
    assert_eq!(small.lent(&rendered), lent, "the lender stays where it is");

    // A large excess: every borrower gives back, and the others are lowered.
    let caches = start();
    let mut ledger = borrow(&caches);
    assert_eq!(ledger.borrowed(&metatile), borrowed);
    let relief = relieve_by(&caches, &mut ledger, 8 * GIB);
    assert_eq!(relief.taken.len(), 2, "{relief:?}");
    assert_eq!(relief.changes.len(), 1, "{relief:?}");
    assert_eq!(relief.changes[0].entry.id, "rendered_cache");
    assert!(ledger.is_empty(), "{ledger:?}");
    let mut caps: HashMap<Key, Ceiling> = relief.caps.into_iter().collect();
    let grant = grant_in(&caches, total, "metatile_cache");
    let mut lifted_at = None;
    let mut last = capacity_of(&caches, "metatile_cache");
    let mut borrows = 0;
    for tick in 0..60 {
        let capped = !caps.is_empty();
        let proposal = real_tick_with(
            &caches,
            total,
            &mut ledger,
            ElasticMode::Apply,
            &mut caps,
            true,
        );
        let now = capacity_of(&caches, "metatile_cache");
        if capped {
            assert_eq!(proposal.held, Some(VALVE_HOLDS));
            assert!(now <= last, "no growth while a ceiling holds: {caches:?}");
        }
        if caps.is_empty() && lifted_at.is_none() {
            lifted_at = Some(tick);
            let plan = grant_in(&caches, total, "rendered_cache");
            assert_eq!(capacity_of(&caches, "rendered_cache"), plan, "its plan");
        }
        // Above its grant by no more than a step for each tick it borrowed.
        borrows += proposal
            .moves
            .iter()
            .filter(|m| id(&m.borrower) == "metatile_cache")
            .count() as u64;
        assert!(
            now <= grant + borrows * proposal.step_bytes,
            "tick {tick}: {caches:?}"
        );
        last = now;
    }
    assert!(lifted_at.is_some());
    assert!(last > grant, "it borrows again once the valve is done");
}

/// #1314 C: `[memory] elastic` is read at every tick. Leaving `apply`
/// ends every loan, and the re-plan tick returns each cache that had one to
/// its grant exactly, inside its 10 % and 64 MiB dead band too (#1473
/// review). Report proposes and resizes nothing; off weighs nothing;
/// entering `apply` again starts from an empty ledger.
#[test]
fn leaving_apply_ends_every_loan_and_off_weighs_nothing() {
    let caches = RefCell::new(vec![
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        sim_cache("rendered_cache", 4096, 512, IDLE),
    ]);
    let total = fitting_total(&caches.borrow());
    let mut ledger = Ledger::default();
    let tick = |ledger: &mut Ledger, mode| {
        real_tick_with(&caches, total, ledger, mode, &mut HashMap::new(), false)
    };
    // One step, inside the dead band: still taken back once apply ends.
    let first = tick(&mut ledger, ElasticMode::Apply);
    assert_eq!(first.moves.len(), 1, "{first:?}");
    let once = capacity_of(&caches, "metatile_cache");
    let grant = grant_in(&caches, total, "metatile_cache");
    assert!(!decide::moves_enough(once, grant), "{once} {grant}");
    let report = tick(&mut ledger, ElasticMode::Report);
    assert!(ledger.is_empty());
    assert_eq!(report.mode, ElasticMode::Report);
    assert!(!report.moves.is_empty(), "still proposed");
    assert_eq!(capacity_of(&caches, "metatile_cache"), grant, "no residue");

    // Many steps: the tick takes them back once apply ends.
    for _ in 0..8 {
        tick(&mut ledger, ElasticMode::Apply);
    }
    let grants = (
        grant_in(&caches, total, "metatile_cache"),
        grant_in(&caches, total, "rendered_cache"),
    );
    assert!(capacity_of(&caches, "metatile_cache") > grants.0 + GIB / 10);
    tick(&mut ledger, ElasticMode::Report);
    assert!(ledger.is_empty());
    assert_eq!(capacity_of(&caches, "metatile_cache"), grants.0);
    assert_eq!(capacity_of(&caches, "rendered_cache"), grants.1);
    let off = tick(&mut ledger, ElasticMode::Off);
    assert_eq!((off.caches.len(), off.moves.len()), (0, 0));
    assert_eq!(capacity_of(&caches, "metatile_cache"), grants.0);

    // Back to apply: from an empty ledger, a step at a time.
    let again = tick(&mut ledger, ElasticMode::Apply);
    assert_eq!(again.moves.len(), 1);
    let bytes = again.moves[0].bytes;
    assert_eq!(capacity_of(&caches, "metatile_cache"), grants.0 + bytes);
    let metatile = (Kind::Cache, "metatile_cache", Scope::Process);
    assert_eq!(ledger.borrowed(&metatile), bytes);
}

// --- #1473 review --------------------------------------------------------

/// What the capacities of the caches with loans sit above their ledger
/// targets, the most of any, over the plan at `total`; and that the
/// headroom elastic memory weighs leaves room for every cache where it is.
fn worst_gap(caches: &RefCell<Vec<SimCache>>, total: u64, ledger: &Ledger) -> i64 {
    let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
    let plan = sim_plan(&rows, total);
    let held: u64 = rows
        .iter()
        .map(|row| {
            let grant = plan.grant(&row.entry).unwrap();
            let target = elastic_grant(ledger, &decide::key(&row.entry), &row.entry, grant);
            target.max(row.usage.capacity_bytes)
        })
        .sum();
    assert!(
        elastic_headroom(&rows, &plan, ledger) <= plan.pool_bytes.saturating_sub(held),
        "headroom counted under what the caches hold"
    );
    let loans: HashSet<Key> = ledger.keys().into_iter().collect();
    rows.iter()
        .filter(|row| loans.contains(&decide::key(&row.entry)))
        .map(|row| {
            let grant = plan.grant(&row.entry).unwrap();
            let target = elastic_grant(ledger, &decide::key(&row.entry), &row.entry, grant);
            row.usage.capacity_bytes as i64 - target as i64
        })
        .max()
        .unwrap_or(0)
}

/// #1473 review: the valve's take-back gives back only the excess, in
/// whole MiB, plus what the borrower holds no data in, not all it
/// borrowed; then borrowing stays off until the process has been under
/// 90 % of its total for the restore's 10 minutes, and resumes a step a
/// tick. Before, a 16 MiB excess took 3.3 GiB off the meta-tile cache,
/// which borrowed it back the next ticks.
#[test]
fn the_take_back_gives_back_the_excess_and_holds_borrowing_off() {
    let metatile = (Kind::Cache, "metatile_cache", Scope::Process);
    let caches = RefCell::new(vec![
        sim_cache("metatile_cache", 1024, 128, thrash(6)),
        sim_cache("rendered_cache", 8192, 512, IDLE),
    ]);
    let total = fitting_total(&caches.borrow()) + 2 * GIB;
    let mut ledger = Ledger::default();
    for _ in 0..40 {
        real_tick(&caches, total, &mut ledger);
    }
    let borrowed = ledger.borrowed(&metatile);
    assert!(borrowed > GIB, "{ledger:?}");
    let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
    let plan = sim_plan(&rows, total);
    let before = capacity_of(&caches, "metatile_cache");
    let resize = |entry: &Entry, bytes: u64| {
        let mut caches = caches.borrow_mut();
        let cache = caches.iter_mut().find(|c| c.key.1 == entry.id).unwrap();
        cache.capacity = bytes;
        Some(bytes)
    };
    let cell = RefCell::new(ledger);
    let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut cell.borrow_mut());
    let excess = 16 * MIB;
    let relief = relief(
        &rows,
        Some(&plan),
        &with_ledger,
        ElasticMode::Apply,
        excess,
        &resize,
    );
    let mut ledger = cell.into_inner();
    assert_eq!(relief.taken.len(), 1, "{relief:?}");
    assert!(relief.changes.is_empty(), "the take-back covered it");
    let after = capacity_of(&caches, "metatile_cache");
    // What it held less what it holds: the excess alone; the capacity it
    // held nothing in went with it.
    let used = rows[0].usage.used_bytes;
    assert_eq!(used.min(before) - used.min(after), excess);
    assert_eq!(before - after, excess + (before - used).div_ceil(MIB) * MIB);
    assert_eq!(ledger.borrowed(&metatile), borrowed - (before - after));
    ledger_holds(&rows, &plan, &ledger).unwrap();

    // Borrowing held off: no borrow, and the cache stays where it is.
    for _ in 0..3 {
        let proposal = real_tick_ruled(
            &caches,
            total,
            &mut ledger,
            ElasticMode::Apply,
            &mut HashMap::new(),
            false,
            (false, true),
        );
        assert!(proposal.moves.iter().all(|m| m.kind != MoveKind::Borrow));
        assert_eq!(capacity_of(&caches, "metatile_cache"), after);
    }
    // Released: a step a tick.
    let proposal = real_tick(&caches, total, &mut ledger);
    let grown = capacity_of(&caches, "metatile_cache") - after;
    assert!(grown > 0 && grown <= proposal.step_bytes, "{grown}");
}

/// #1473 review: a take-back holds borrowing off until the process has
/// been under 90 % of its total for 10 minutes; returns need only under
/// 90 %; at 90 % or more neither.
#[test]
fn a_take_back_holds_borrowing_off_for_the_restores_ten_minutes() {
    let start = Instant::now();
    let mut governor = Governor {
        borrow_held: true,
        sample: Some((80, 100)),
        ..Governor::default()
    };
    governor.pressure.observe(start, 80, 100);
    assert_eq!(tick_rules(&mut governor, start), (false, true));
    let almost = start + decide::UNDER_FOR - Duration::from_secs(1);
    governor.pressure.observe(almost, 80, 100);
    assert_eq!(tick_rules(&mut governor, almost), (false, true));
    let after = start + decide::UNDER_FOR;
    governor.pressure.observe(after, 80, 100);
    assert_eq!(tick_rules(&mut governor, after), (true, true));
    assert!(!governor.borrow_held);
    governor.sample = Some((95, 100));
    assert_eq!(tick_rules(&mut governor, after), (false, false));
    governor.sample = None;
    assert_eq!(tick_rules(&mut governor, after), (true, true), "no valve");
}

/// #1473 review: a cache that decayed while idle, read again, takes back
/// what it lent the pool at the restore's pace, 5 % of its grant a tick,
/// with no ghost hits needed, while the process is under 90 % of its
/// total; at 90 % or more, or while still idle, nothing.
#[test]
fn a_cache_read_again_takes_back_what_it_lent_the_pool() {
    let pool = 8 * GIB;
    let decayed = |capacity_mib: u64, lent_mib: u64| Elastic {
        capacity: capacity_mib * MIB,
        grant: GIB,
        floor: 128 * MIB,
        ceiling: 8 * GIB,
        offset: -(lent_mib as i64 * MIB as i64),
        pool_lent: lent_mib * MIB,
        used: capacity_mib * MIB,
        idle: false,
        value: 0.0,
        ghost_hits: 0,
        ..elastic_cache("metatile_cache", capacity_mib, 0.0, 0)
    };
    let other = Elastic {
        lend_value: 1e-12,
        value: 0.0,
        ghost_hits: 0,
        ..elastic_cache("rendered_cache", 4096, 0.0, 0)
    };
    let headroom = 896 * MIB;
    let rules = |may_borrow, may_return| Rules {
        may_borrow,
        may_return,
        decays: true,
    };
    let at_90 = elastic(
        &[decayed(128, 896), other.clone()],
        pool,
        headroom,
        rules(false, false),
    );
    assert_eq!(at_90, [], "at 90 % or more");
    let idle = Elastic {
        idle: true,
        ..decayed(128, 896)
    };
    assert!(elastic(&[idle], pool, headroom, rules(true, true))
        .iter()
        .all(|m| m.kind != MoveKind::Return));
    let (mut capacity, mut lent, mut ticks) = (128, 896, 0);
    while lent > 0 {
        let moves = elastic(
            &[decayed(capacity, lent), other.clone()],
            pool,
            headroom,
            rules(false, true),
        );
        let m = &moves[0];
        assert_eq!((m.kind, id(&m.lender)), (MoveKind::Return, "pool"));
        assert!(m.bytes <= 52 * MIB, "5 % of its grant, in whole MiB");
        capacity += m.bytes / MIB;
        lent -= m.bytes / MIB;
        ticks += 1;
        assert!(ticks <= 20);
    }
    assert_eq!(
        (capacity, ticks),
        (1024, 18),
        "back to its grant in 90 minutes"
    );
    // Never more than the headroom left.
    let moves = elastic(
        &[decayed(128, 896), other],
        pool,
        10 * MIB,
        rules(true, true),
    );
    assert_eq!(moves[0].bytes, 10 * MIB);
}

/// #1473 review, through the real tick: what the ledger sheds as a total
/// shrinks reaches the caches: a cache with loans is held to its grant
/// plus its offset exactly, never left in the tick's dead band above it,
/// and the headroom counts every cache where it is, the dead band's
/// slack included; with or without a cache filled ahead of its reads.
#[test]
fn the_ledgers_sheds_reach_the_caches() {
    for prewarmed in [false, true] {
        let caches = RefCell::new(vec![
            sim_cache("metatile_cache", 1024, 128, thrash(3)),
            SimCache {
                prewarmed,
                ..sim_cache("rendered_cache", 4096, 512, IDLE)
            },
        ]);
        let mut total = fitting_total(&caches.borrow()) + 2 * GIB;
        let mut ledger = Ledger::default();
        for _ in 0..30 {
            real_tick(&caches, total, &mut ledger);
            assert!(worst_gap(&caches, total, &ledger) < 64 * MIB as i64);
        }
        for _ in 0..24 {
            total -= 48 * MIB;
            real_tick(&caches, total, &mut ledger);
            let worst = worst_gap(&caches, total, &ledger);
            assert!(worst < 64 * MIB as i64, "{} MiB over", worst / MIB as i64);
        }
    }
}

/// #1473 review: a borrower whose shrink in place is refused gives back
/// nothing: its loans stay, and the relief reports the refusal.
#[test]
fn a_refused_give_back_keeps_its_loans_and_is_reported() {
    let metatile = (Kind::Cache, "metatile_cache", Scope::Process);
    let caches = RefCell::new(vec![
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        sim_cache("rendered_cache", 4096, 512, IDLE),
    ]);
    let total = fitting_total(&caches.borrow());
    let mut ledger = Ledger::default();
    for _ in 0..10 {
        real_tick(&caches, total, &mut ledger);
    }
    let borrowed = ledger.borrowed(&metatile);
    assert!(borrowed > 0);
    let before = capacity_of(&caches, "metatile_cache");
    let rows: Vec<Row> = caches.borrow().iter().map(sim_row).collect();
    let plan = sim_plan(&rows, total);
    // Every cache refuses its shrink in place: the 90 % relief that
    // follows lowers nothing either, so no lender's loans end.
    let resize = |entry: &Entry, bytes: u64| {
        let mut caches = caches.borrow_mut();
        let cache = caches.iter_mut().find(|c| c.key.1 == entry.id).unwrap();
        cache.capacity = cache.capacity.max(bytes);
        Some(cache.capacity)
    };
    let cell = RefCell::new(ledger);
    let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut cell.borrow_mut());
    let relief = relief(
        &rows,
        Some(&plan),
        &with_ledger,
        ElasticMode::Apply,
        MIB,
        &resize,
    );
    let ledger = cell.into_inner();
    assert!(relief.taken.is_empty() && relief.changes.is_empty());
    assert_eq!(relief.refused[0].entry.id, "metatile_cache", "{relief:?}");
    assert_eq!(capacity_of(&caches, "metatile_cache"), before);
    assert_eq!(ledger.borrowed(&metatile), borrowed, "its loans stay");
    ledger.balanced().unwrap();
}

/// #1473 review: a cache its engines fill ahead of its reads
/// (`Churn::prewarmed`) neither lends nor decays: no move takes from it,
/// even worth nothing.
#[test]
fn a_prewarmed_cache_neither_lends_nor_decays() {
    let caches = vec![
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        SimCache {
            prewarmed: true,
            ..sim_cache("rendered_cache", 4096, 512, IDLE)
        },
    ];
    let rows: Vec<Row> = caches.iter().map(sim_row).collect();
    let plan = sim_plan(&rows, fitting_total(&caches));
    let weighed = elastic_inputs(&rows, &plan, &Ledger::default());
    let prewarmed = weighed
        .iter()
        .find(|c| c.key.1 == "rendered_cache")
        .unwrap();
    assert!(!prewarmed.lends && !prewarmed.decays);
    let decays = Rules {
        may_borrow: true,
        may_return: true,
        decays: true,
    };
    for m in elastic(&weighed, plan.pool_bytes, 0, decays) {
        assert_ne!(id(&m.lender), "rendered_cache", "{m:?}");
    }
}

/// #1473 review: `/metrics` shows each cache's borrowed and lent bytes,
/// summed over its collections; a cache with no loan has no series.
#[test]
fn the_ledger_is_shown_on_metrics() {
    let collection = |id: &str| Party {
        key: (
            Kind::Cache,
            "offset_test_cache",
            Scope::Collection {
                engine: "geotiff",
                id: id.into(),
            },
        ),
        source: SizeSource::Default,
        configured: GIB,
    };
    let mut ledger = Ledger::default();
    ledger.book(Some(collection("a")), None, 64 * MIB);
    ledger.book(Some(collection("b")), None, 32 * MIB);
    ledger.book(None, Some(party("offset_test_lender_cache", GIB)), 16 * MIB);
    show_offsets(&ledger);
    let gauge = |cache: &str, role: &str| ELASTIC_OFFSETS.with_label_values(&[cache, role]).get();
    assert_eq!(gauge("offset_test_cache", "borrowed"), 96 * MIB as i64);
    assert_eq!(gauge("offset_test_lender_cache", "lent"), 16 * MIB as i64);
    show_offsets(&Ledger::default());
    assert_eq!(gauge("offset_test_cache", "borrowed"), 0, "reset");
}

/// #1473 review: the ledger's invariant names the limit it hit: the pool,
/// or the plan's own expected use where that is past the pool already.
#[test]
fn a_broken_ledger_names_the_limit_it_passed() {
    let caches = vec![
        sim_cache("metatile_cache", 1024, 128, thrash(3)),
        sim_cache("rendered_cache", 4096, 512, IDLE),
    ];
    let rows: Vec<Row> = caches.iter().map(sim_row).collect();
    let mut ledger = Ledger::default();
    ledger.book(Some(party("metatile_cache", GIB)), None, 64 * GIB);
    let roomy = sim_plan(&rows, fitting_total(&caches));
    assert!(roomy.pool_bytes >= roomy.expected_bytes);
    let error = ledger_holds(&rows, &roomy, &ledger).unwrap_err();
    assert!(error.contains("past the pool of"), "{error}");
    let tight = sim_plan(&rows, 512 * MIB);
    assert!(tight.expected_bytes > tight.pool_bytes, "{tight:?}");
    let error = ledger_holds(&rows, &tight, &ledger).unwrap_err();
    assert!(
        error.contains("past the plan's own expected use"),
        "{error}"
    );
}

/// #1473 review: a working-set borrower whose plan counts it at its need
/// borrows from a working-set lender's unused capacity, and fills what it
/// gets; through real ticks, what the caches hold never passes the pool,
/// and the tick never has to claw it back.
#[test]
fn a_working_set_borrower_never_fills_past_the_pool() {
    let caches = RefCell::new(vec![
        sim_cache("grid_cache", 1024, 64, thrash(4)),
        sim_cache("tile_cache", 4096, 64, IDLE),
    ]);
    let row_of = |c: &SimCache| {
        let mut row = sim_row(c);
        row.demand = Some(Demand {
            bytes: 256 * MIB,
            rule: "probe",
            floor: false,
            ahead: false,
            setting: "probe",
            parts: Vec::new(),
        });
        row
    };
    let rows_now = || caches.borrow().iter().map(row_of).collect::<Vec<Row>>();
    let set = |id: &str, bytes: u64| {
        for cache in caches.borrow_mut().iter_mut() {
            if cache.key.1 == id {
                cache.capacity = bytes;
            }
        }
    };
    let rows = rows_now();
    let mut total = caches.borrow().iter().map(|c| c.configured).sum::<u64>();
    while rows
        .iter()
        .any(|row| sim_plan(&rows, total).grant(&row.entry).unwrap().bytes < row.configured_bytes)
    {
        total += 64 * MIB;
    }
    let mut ledger = Ledger::default();
    let mut shrinks = 0;
    for tick in 0..30 {
        let rows = rows_now();
        let plan = sim_plan(&rows, total);
        keep_ledger(&mut ledger, ElasticMode::Apply, &rows, Some(&plan));
        for row in &rows {
            let key = decide::key(&row.entry);
            let grant = plan.grant(&row.entry).unwrap();
            let target = elastic_grant(&ledger, &key, &row.entry, grant);
            let floor = floor_of(&row.entry, grant);
            if let Some(to) =
                tick_target(row.usage.capacity_bytes, target, floor, None, false, false)
            {
                if to < row.usage.capacity_bytes && row.entry.id == "grid_cache" {
                    shrinks += 1;
                }
                set(row.entry.id, to);
            }
        }
        let rows = rows_now();
        let resize = |entry: &Entry, bytes: u64| {
            set(entry.id, bytes);
            Some(bytes)
        };
        let cell = RefCell::new(std::mem::take(&mut ledger));
        let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut cell.borrow_mut());
        run_elastic(
            &rows,
            &plan,
            &with_ledger,
            ElasticMode::Apply,
            false,
            (true, true),
            &resize,
        );
        ledger = cell.into_inner();
        let held: u64 = rows_now().iter().map(|r| r.usage.used_bytes).sum();
        assert!(held <= plan.pool_bytes, "tick {tick}");
    }
    assert_eq!(shrinks, 0, "the tick clawed nothing back");
    let rows = rows_now();
    let plan = sim_plan(&rows, total);
    let held: u64 = rows.iter().map(|r| r.usage.used_bytes).sum();
    assert!(
        held <= plan.pool_bytes,
        "the caches hold {} MiB of a {} MiB pool",
        held / MIB,
        plan.pool_bytes / MIB
    );
}

/// #1473 review: the headroom elastic memory moves within counts each cache
/// where it is: one the tick's dead band left above its grant takes that
/// much off, though the ledger's targets say less.
#[test]
fn the_headroom_counts_a_cache_above_its_target_where_it_is() {
    // A working-set cache counted at its need leaves headroom in the pool.
    let caches = vec![
        sim_cache("grid_cache", 1024, 64, thrash(4)),
        sim_cache("tile_cache", 4096, 64, IDLE),
    ];
    let row_of = |c: &SimCache| Row {
        demand: Some(Demand {
            bytes: 256 * MIB,
            ..demand(256 * MIB, MIB)
        }),
        ..sim_row(c)
    };
    let rows: Vec<Row> = caches.iter().map(row_of).collect();
    let plan = sim_plan(&rows, fitting_total(&caches) + GIB);
    let ledger = Ledger::default();
    let at_grants = elastic_headroom(&rows, &plan, &ledger);
    assert!(at_grants > GIB, "{at_grants}");
    // The busy one, held at its grant: counted at its capacity.
    let grant = plan.grant(&rows[0].entry).unwrap().bytes;
    let mut above = caches.clone();
    above[0].capacity = grant + 300 * MIB;
    let rows: Vec<Row> = above.iter().map(row_of).collect();
    assert_eq!(expected_use(&rows, &plan, &ledger), plan.expected_bytes);
    assert_eq!(
        elastic_headroom(&rows, &plan, &ledger),
        at_grants - 300 * MIB
    );
}
