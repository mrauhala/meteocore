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
    let relief = relief_block(9 * GIB, 8 * GIB, moved, &[], true);
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
    let relief = relief_block(9 * GIB, 8 * GIB, &lowered, &[refused], true);
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
    let document = json();
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
