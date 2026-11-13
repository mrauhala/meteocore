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
        tick_target(applied, doubled, None, false, None),
        Some(doubled),
        "the next tick applies it"
    );
    for field_mb in [24, 26] {
        let jittered = grid_grant(field_mb);
        assert_ne!(jittered, applied);
        assert_eq!(
            tick_target(applied, jittered, None, false, None),
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

/// A resize that stopped short is not tried again while its capacity and
/// grant are what they were; the valve's ceiling caps a grant however
/// little it moves; a lifted ceiling gives the grant at once.
#[test]
fn the_tick_skips_a_refused_resize_and_follows_the_valves_ceiling() {
    let refused = Some((300 * MIB, 100 * MIB));
    assert_eq!(
        tick_target(300 * MIB, 100 * MIB, None, false, refused),
        None
    );
    assert_eq!(
        tick_target(400 * MIB, 100 * MIB, None, false, refused),
        Some(100 * MIB),
        "the capacity moved since"
    );
    assert_eq!(
        tick_target(1000 * MIB, 1010 * MIB, Some(900 * MIB), false, None),
        Some(900 * MIB)
    );
    assert_eq!(
        tick_target(900 * MIB, 1000 * MIB, Some(950 * MIB), false, None),
        Some(950 * MIB),
        "a restore step of 5 %"
    );
    assert_eq!(
        tick_target(950 * MIB, 1000 * MIB, None, true, None),
        Some(1000 * MIB),
        "lifted"
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
    let mut caps: HashMap<Key, u64> = HashMap::new();
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
                    caps.insert(key.clone(), target);
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
                caps.get(&key).copied(),
                lifted.contains(&key),
                None,
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
    let mut caps: HashMap<Key, u64> = [
        (key("metatile_cache"), 1000 * MIB),
        (key("rendered_cache"), 250 * MIB),
        (key("gone_cache"), 10 * MIB),
    ]
    .into();
    let grants: HashMap<Key, u64> = [
        (key("metatile_cache"), 2000 * MIB),
        (key("rendered_cache"), 256 * MIB),
    ]
    .into();
    let (raised, lifted) = restore(&mut caps, &|k| grants.get(k).copied());
    assert_eq!(raised, vec![key("metatile_cache")]);
    assert_eq!(caps[&key("metatile_cache")], 1100 * MIB);
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
    let relief = relief_block(9 * GIB, 8 * GIB, moved, true);
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

/// #1315: the governor's step on a running server. Gated in report mode;
/// in auto with `total_mb`, it samples resident memory every step and fits
/// the plan again 5 minutes after the last fit, a reload's included; it
/// skips while a reload holds `reload_lock`; and `/admin/cache-plan`
/// reports its last tick. Back in report mode it is gated again, its
/// ceilings dropped.
///
/// Runs in a child process: it resizes the process-wide caches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_governor_is_gated_ticks_and_skips_while_a_reload_holds_the_lock() {
    const CHILD: &str = "MC_TEST_GOVERNOR_CHILD";
    const NAME: &str =
        "governor::tests::the_governor_is_gated_ticks_and_skips_while_a_reload_holds_the_lock";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "{NAME} failed in its child process");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let radar = format!(
        "[[collections]]\nid = \"radar\"\ntitle = \"t\"\ndescription = \"d\"\n\
         engine_type = \"geotiff\"\ndata_path = {:?}\napis = [\"wms\", \"edr\"]\n\
         [collections.geotiff]\nfilename_template = \"radar_%Y%m%dT%H%MZ.tif\"\n\
         parameter = \"reflectivity\"\nunit = \"dBZ\"\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/radar")
    );
    let write = |memory: &str| {
        std::fs::write(
            &config_path,
            format!("[server]\nhost = \"127.0.0.1\"\nport = 8000\n{memory}{radar}"),
        )
        .unwrap();
    };
    write("");
    let state = crate::watcher::tests::build_state(&config_path);
    cache_plan::log(&state, cache_plan::Occasion::Boot);
    let start = Instant::now();
    assert_eq!(step(&state, start), Step::Gated, "report mode");

    write("[memory]\nmode = \"auto\"\ntotal_mb = 8192\n");
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

    governor().caps.insert(
        decide::key(&Entry::cache(
            "metatile_cache",
            Scope::Process,
            SizeSource::Default,
        )),
        512 * MIB,
    );
    write("");
    crate::admin::do_reload(&state).unwrap();
    assert_eq!(step(&state, later + 2 * REPLAN_EVERY), Step::Gated);
    assert!(governor().caps.is_empty(), "report mode drops the ceilings");
}
