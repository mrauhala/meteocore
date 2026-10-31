use std::path::Path;

use ds_cache::plan::grant::{GIB, MIN_TOTAL};
use ds_cache::plan::{Demand, Entry, Usage};
use ds_cache::{Churn, MIB};

use super::detect::{self, Limit, Reading, Source};
use super::*;

/// A fixture tree of `/proc` and `/sys` files under a temporary root.
struct Tree(tempfile::TempDir);

impl Tree {
    fn new() -> Self {
        Tree(tempfile::tempdir().unwrap())
    }

    fn file(&self, path: &str, text: &str) -> &Self {
        let path = self.0.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        self
    }

    /// A host of `total` GiB with `available` GiB available.
    fn meminfo(&self, total: u64, available: u64) -> &Self {
        let kib = |gib: u64| gib * GIB / 1024;
        self.file(
            "proc/meminfo",
            &format!(
                "MemTotal:       {} kB\nMemFree:         1024 kB\nMemAvailable:   {} kB\n",
                kib(total),
                kib(available)
            ),
        )
    }

    fn read(&self) -> Reading {
        detect::read(self.0.path())
    }
}

/// The cgroup v2 hierarchy at `/sys/fs/cgroup`, as systemd and Docker
/// mount it.
const V2_MOUNT: &str = "30 23 0:26 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:4 - \
                        cgroup2 cgroup2 rw,nsdelegate,memory_recursiveprot\n";

fn bytes(gib: u64) -> String {
    format!("{}\n", gib * GIB)
}

fn limit(version: u8, file: &'static str, gib: u64) -> Option<Limit> {
    Some(Limit {
        version,
        file,
        bytes: gib * GIB,
    })
}

/// #1312 acceptance: a container's cgroup v2 with a limit, in its own
/// cgroup namespace (the path is `/`). The total is 80 % of it, and
/// `memory.low` is reported.
#[test]
fn a_cgroup_v2_limit_sets_the_total() {
    let tree = Tree::new();
    tree.meminfo(16, 12)
        .file("proc/self/cgroup", "0::/\n")
        .file("proc/self/mountinfo", V2_MOUNT)
        .file("sys/fs/cgroup/memory.max", &bytes(4))
        .file("sys/fs/cgroup/memory.high", "max\n")
        .file("sys/fs/cgroup/memory.low", &bytes(1));
    let reading = tree.read();
    assert_eq!(reading.limit, limit(2, "memory.max", 4));
    assert_eq!(reading.low_bytes, Some(GIB));
    assert_eq!(reading.host_bytes, Some(16 * GIB));
    let total = detect::total(None, &reading).unwrap();
    assert_eq!(total.bytes, 3276 * MIB);
    assert_eq!(
        total.source.to_string(),
        "cgroup v2 memory.max 4.00 GiB × 0.8"
    );
    assert!(total.warnings.is_empty(), "{:?}", total.warnings);
}

/// #1312 acceptance: `max` is no limit: the host's memory sets the total,
/// with a WARN naming it and `total_mb`.
#[test]
fn a_cgroup_v2_max_is_no_limit() {
    let tree = Tree::new();
    tree.meminfo(16, 12)
        .file("proc/self/cgroup", "0::/\n")
        .file("proc/self/mountinfo", V2_MOUNT)
        .file("sys/fs/cgroup/memory.max", "max\n")
        .file("sys/fs/cgroup/memory.high", "max\n");
    let reading = tree.read();
    assert_eq!(reading.limit, None);
    let total = detect::total(None, &reading).unwrap();
    assert_eq!(total.bytes, 8 * GIB, "min(16 × 0.5, 12 × 0.8)");
    assert_eq!(
        total.source,
        Source::Host {
            host_bytes: 16 * GIB,
            available_bytes: Some(12 * GIB)
        }
    );
    assert_eq!(
        total.warnings,
        [
            "No memory limit: the memory plan takes 8.00 GiB (min(MemTotal 16.00 GiB × 0.5, \
          MemAvailable at boot 12.00 GiB × 0.8)). Set [memory] total_mb to the memory this \
          server may use"
        ]
    );
}

/// #1312 acceptance: nested v2 limits, the smallest wins: a systemd
/// slice's `memory.max` below its service's. Without a mountinfo, the
/// hierarchy is at `/sys/fs/cgroup`.
#[test]
fn the_smallest_of_nested_cgroup_v2_limits_wins() {
    let tree = Tree::new();
    tree.meminfo(64, 60)
        .file("proc/self/cgroup", "0::/system.slice/meteocore.service\n")
        .file(
            "sys/fs/cgroup/system.slice/meteocore.service/memory.max",
            &bytes(8),
        )
        .file("sys/fs/cgroup/system.slice/memory.max", &bytes(6))
        .file("sys/fs/cgroup/memory.max", &bytes(2));
    // The mount point's own file is an ancestor's too.
    assert_eq!(tree.read().limit, limit(2, "memory.max", 2));
    std::fs::remove_file(tree.0.path().join("sys/fs/cgroup/memory.max")).unwrap();
    assert_eq!(tree.read().limit, limit(2, "memory.max", 6));
}

/// #1312 acceptance: `memory.high` below `memory.max` sets the total.
#[test]
fn cgroup_v2_memory_high_below_memory_max_wins() {
    let tree = Tree::new();
    tree.meminfo(64, 60)
        .file("proc/self/cgroup", "0::/kubepods/pod1/c1\n")
        .file("proc/self/mountinfo", V2_MOUNT)
        .file("sys/fs/cgroup/kubepods/pod1/c1/memory.max", &bytes(8))
        .file("sys/fs/cgroup/kubepods/pod1/c1/memory.high", &bytes(6));
    let reading = tree.read();
    assert_eq!(reading.limit, limit(2, "memory.high", 6));
    assert_eq!(
        detect::total(None, &reading).unwrap().source.to_string(),
        "cgroup v2 memory.high 6.00 GiB × 0.8"
    );
}

/// The v1 memory controller of a Docker container without a cgroup
/// namespace: `/proc/self/cgroup` names the host's path, and the mount's
/// root is that path.
const V1_MOUNT: &str = "35 29 0:31 /docker/abc /sys/fs/cgroup/memory ro,nosuid,nodev,noexec,\
                        relatime master:15 - cgroup cgroup rw,memory\n";

const V1_CGROUP: &str = "12:memory:/docker/abc\n11:cpu,cpuacct:/docker/abc\n";

/// v1 "unlimited".
const UNLIMITED: &str = "9223372036854771712\n";

/// #1312 acceptance: cgroup v1 with a limit, `memory.limit_in_bytes` or
/// `hierarchical_memory_limit`, whichever is smaller.
#[test]
fn a_cgroup_v1_limit_sets_the_total() {
    let tree = Tree::new();
    tree.meminfo(32, 30)
        .file("proc/self/cgroup", V1_CGROUP)
        .file("proc/self/mountinfo", V1_MOUNT)
        .file("sys/fs/cgroup/memory/memory.limit_in_bytes", &bytes(2))
        .file(
            "sys/fs/cgroup/memory/memory.stat",
            &format!("cache 0\nhierarchical_memory_limit {UNLIMITED}"),
        )
        .file("sys/fs/cgroup/memory/memory.soft_limit_in_bytes", &bytes(1));
    let reading = tree.read();
    assert_eq!(reading.limit, limit(1, "memory.limit_in_bytes", 2));
    assert_eq!(reading.low_bytes, Some(GIB));
    assert_eq!(
        detect::total(None, &reading).unwrap().source.to_string(),
        "cgroup v1 memory.limit_in_bytes 2.00 GiB × 0.8"
    );
    tree.file("sys/fs/cgroup/memory/memory.limit_in_bytes", UNLIMITED)
        .file(
            "sys/fs/cgroup/memory/memory.stat",
            &format!("cache 0\nhierarchical_memory_limit {}", 3 * GIB),
        );
    assert_eq!(tree.read().limit, limit(1, "hierarchical_memory_limit", 3));
}

/// #1312 acceptance: v1 "unlimited", or any value at or above `MemTotal`,
/// is no limit.
#[test]
fn cgroup_v1_unlimited_is_no_limit() {
    let tree = Tree::new();
    tree.meminfo(32, 30)
        .file("proc/self/cgroup", V1_CGROUP)
        .file("proc/self/mountinfo", V1_MOUNT)
        .file("sys/fs/cgroup/memory/memory.limit_in_bytes", UNLIMITED)
        .file(
            "sys/fs/cgroup/memory/memory.stat",
            &format!("hierarchical_memory_limit {UNLIMITED}"),
        );
    assert_eq!(tree.read().limit, None);
    tree.file("sys/fs/cgroup/memory/memory.limit_in_bytes", &bytes(32));
    assert_eq!(tree.read().limit, None, "MemTotal itself");
    let total = detect::total(None, &tree.read()).unwrap();
    assert_eq!(total.bytes, 16 * GIB);
}

/// #1312 acceptance: a hybrid host, v2 at `/sys/fs/cgroup/unified` and the
/// memory controller on v1: v2 is read first and has no limit, v1's sets
/// it; once v2 has one, v2's wins.
#[test]
fn a_hybrid_host_reads_v2_first() {
    let tree = Tree::new();
    tree.meminfo(32, 30)
        .file(
            "proc/self/cgroup",
            "0::/user.slice/x\n12:memory:/user.slice/x\n",
        )
        .file(
            "proc/self/mountinfo",
            "30 23 0:26 / /sys/fs/cgroup/unified rw,nosuid shared:4 - cgroup2 cgroup2 rw\n\
             35 29 0:31 / /sys/fs/cgroup/memory rw,nosuid shared:15 - cgroup cgroup rw,memory\n",
        )
        .file("sys/fs/cgroup/unified/user.slice/x/cgroup.procs", "1\n")
        .file(
            "sys/fs/cgroup/memory/user.slice/x/memory.limit_in_bytes",
            &bytes(3),
        );
    assert_eq!(tree.read().limit, limit(1, "memory.limit_in_bytes", 3));
    tree.file("sys/fs/cgroup/unified/user.slice/x/memory.max", &bytes(5));
    assert_eq!(tree.read().limit, limit(2, "memory.max", 5));
}

/// #1312 acceptance: no cgroup: the host's memory, the smaller of half of
/// it and 80 % of what is available at boot.
#[test]
fn without_a_cgroup_the_host_sets_the_total() {
    let tree = Tree::new();
    tree.meminfo(16, 6);
    let reading = tree.read();
    assert_eq!((reading.limit.clone(), reading.low_bytes), (None, None));
    let total = detect::total(None, &reading).unwrap();
    assert_eq!(total.bytes / MIB, 4915, "min(8, 4.8) GiB");
    assert_eq!(total.warnings.len(), 1);
    // Without MemAvailable, half of MemTotal.
    tree.file(
        "proc/meminfo",
        &format!("MemTotal: {} kB\n", 16 * GIB / 1024),
    );
    let total = detect::total(None, &tree.read()).unwrap();
    assert_eq!(total.bytes, 8 * GIB);
    assert_eq!(total.source.to_string(), "MemTotal 16.00 GiB × 0.5");
}

/// `[memory] total_mb` wins, with a WARN when it is above the cgroup
/// limit; under 512 MiB the plan warns that it is at floors; with nothing
/// known there is no total.
#[test]
fn total_mb_wins_and_warns_above_the_limit() {
    let reading = Reading {
        limit: limit(2, "memory.max", 4),
        host_bytes: Some(16 * GIB),
        ..Reading::default()
    };
    let total = detect::total(Some(2048), &reading).unwrap();
    assert_eq!((total.bytes, &total.source), (2 * GIB, &Source::Configured));
    assert!(total.warnings.is_empty());
    let total = detect::total(Some(6144), &reading).unwrap();
    assert_eq!(total.bytes, 6 * GIB);
    assert_eq!(
        total.warnings,
        [
            "[memory] total_mb = 6144 is above the cgroup v2 memory.max of 4.00 GiB: the \
          process is killed when it crosses the limit"
        ]
    );
    let small = detect::total(Some(300), &Reading::default()).unwrap();
    assert!(small.bytes < MIN_TOTAL);
    assert_eq!(
        small.warnings,
        [
            "A memory total of 300 MiB ([memory] total_mb) is under 512 MiB: the plan puts \
          every cache and budget at its floor"
        ]
    );
    assert_eq!(detect::total(None, &Reading::default()), None);
}

/// Where the root has no `/proc/meminfo`, macOS and Windows ask the
/// system; elsewhere nothing is known.
#[test]
fn the_host_memory_comes_from_the_system_off_linux() {
    let reading = detect::read(Path::new("/nonexistent-root"));
    assert_eq!(reading.limit, None);
    if cfg!(any(target_os = "macos", windows)) {
        let total = reading.host_bytes.expect("the system's memory");
        assert!(total >= 512 * MIB, "{total}");
        assert!(reading.available_bytes.unwrap() <= total);
    } else {
        assert_eq!(reading.host_bytes, None);
    }
}

fn collections(toml: &str) -> HashMap<String, CollectionConfig> {
    let config: ds_core::config::ServerConfig =
        toml::from_str(&format!("[server]\nhost = \"h\"\nport = 1\n{toml}")).unwrap();
    config
        .collections
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect()
}

fn row(id: &'static str, scope: Scope, source: SizeSource, capacity: u64) -> Row {
    let cache = id.ends_with("_cache");
    Row {
        entry: Entry {
            id,
            kind: if cache { Kind::Cache } else { Kind::Budget },
            scope,
            source,
        },
        usage: Usage {
            capacity_bytes: capacity,
            churn: cache.then(Churn::default),
            ..Usage::default()
        },
        window: None,
        demand: None,
    }
}

const RADAR: &str = "[[collections]]\nid = \"radar\"\ntitle = \"t\"\ndescription = \"d\"\n\
                     engine_type = \"geotiff\"\napis = [\"wms\", \"edr\"]\n";

/// Rule 2: what no collection can reach, from the config: the engines
/// present, their APIs, a local GRIB source, a cache built empty.
#[test]
fn unreachable_caches_follow_the_config() {
    let local_grib = "[[collections]]\nid = \"local\"\ntitle = \"t\"\ndescription = \"d\"\n\
                      engine_type = \"grib\"\napis = [\"edr\"]\n[collections.grib]\n\
                      data_path = \"/data/grib\"\n";
    let remote_grib = "[[collections]]\nid = \"remote\"\ntitle = \"t\"\ndescription = \"d\"\n\
                       engine_type = \"grib\"\napis = [\"edr\"]\n[collections.grib]\n\
                       data_path = \"s3://bucket/grib\"\n";
    let config = collections(&format!("{RADAR}{local_grib}{remote_grib}"));
    let why = |id, scope, source, capacity| unreachable(&row(id, scope, source, capacity), &config);
    let process = |id| why(id, Scope::Process, SizeSource::Default, MIB);
    assert_eq!(process("metatile_cache"), None);
    assert_eq!(process("rendered_cache"), None);
    assert_eq!(process("render_budget"), None);
    assert_eq!(process("edr_response"), None);
    assert_eq!(
        process("tiles3d_content_cache").as_deref(),
        Some("no 3dtiles collection")
    );
    assert_eq!(
        process("vector_tile_cache").as_deref(),
        Some("no vector tile collection")
    );
    assert_eq!(
        process("station_frame_cache").as_deref(),
        Some("no station symbol layer")
    );
    let engine = |id, engine| why(id, Scope::Engine(engine), SizeSource::Default, MIB);
    assert_eq!(engine("geotiff_decoded_chunk_cache", "geotiff"), None);
    assert_eq!(
        engine("odim_composite_cache", "odim").as_deref(),
        Some("no odim collection")
    );
    assert_eq!(
        engine("pvol_voxel_grid_cache", "odim-volume").as_deref(),
        Some("no 3dtiles collection")
    );
    assert_eq!(
        engine("zarr_read", "zarr").as_deref(),
        Some("no zarr collection")
    );
    let message = |id: &str| {
        let scope = Scope::Source {
            engine: "grib",
            id: id.into(),
        };
        why("grib_message_cache", scope, SizeSource::Default, 0)
    };
    assert_eq!(
        message("local").as_deref(),
        Some("a local source reads through the page cache")
    );
    assert_eq!(message("remote"), None);
    let tile = |source| {
        let scope = Scope::Collection {
            engine: "geotiff",
            id: "radar".into(),
        };
        why("tile_cache", scope, source, 0)
    };
    assert_eq!(
        tile(SizeSource::Constant).as_deref(),
        Some("built empty for its source")
    );
    assert_eq!(tile(SizeSource::Default), None, "disabled, not unreachable");
}

/// A plan that cuts logs one WARN block: the total and its source, the
/// sums, each cut with its sizes and step, and the largest demands.
#[test]
fn a_plan_that_cuts_warns_with_each_cut() {
    let satellite = "[[collections]]\nid = \"sat\"\ntitle = \"t\"\ndescription = \"d\"\n\
                engine_type = \"satellite\"\napis = [\"wms\"]\n";
    let config = collections(&format!("{RADAR}{satellite}"));
    let mut scans = row(
        "satellite_frame_cache",
        Scope::Engine("satellite"),
        SizeSource::Default,
        1024 * MIB,
    );
    let part =
        ds_cache::plan::CacheDemand::demand(300 * MIB, "r", "s").input("scan_bytes", 30 * MIB);
    scans.demand = Some(Demand {
        bytes: part.bytes,
        rule: part.rule,
        floor: false,
        ahead: true,
        setting: part.setting,
        parts: vec![("sat".into(), part)],
    });
    let rows = vec![
        row(
            "metatile_cache",
            Scope::Process,
            SizeSource::Default,
            1024 * MIB,
        ),
        scans,
        row(
            "render_budget",
            Scope::Process,
            SizeSource::Default,
            1024 * MIB,
        ),
    ];
    let reading = Reading {
        host_bytes: Some(2 * GIB),
        ..Reading::default()
    };
    let report = report(None, &reading, &rows, &config, Baseline::Estimated);
    let block = warn_block(&report, &rows).expect("the plan cuts");
    let lines: Vec<&str> = block.lines().collect();
    assert_eq!(
        lines[0],
        "Memory plan: a total of 1.00 GiB (MemTotal 2.00 GiB × 0.5) does not hold every cache \
         and budget at its size today: demand 0.29 GiB, floors 0.24 GiB, pins 0.00 GiB. No \
         memory limit was found, so the total is a share of the host's memory: set [memory] \
         total_mb to the memory this server may use"
    );
    let metatile = report.plan.as_ref().unwrap().grant(&rows[0].entry).unwrap();
    assert_eq!(
        lines[1],
        format!(
            "  metatile_cache 1024 MiB → {} (cut: traffic)",
            mib(metatile.bytes)
        )
    );
    assert_eq!(lines[2], "  largest demand: sat 300 MiB");
    assert!(lines[3].starts_with("  A report: no size is changed."));
    // From a cgroup limit, or total_mb, the total is no guess.
    let limited = Reading {
        limit: limit(2, "memory.max", 1),
        ..Reading::default()
    };
    let report = super::report(None, &limited, &rows, &config, Baseline::Estimated);
    let block = warn_block(&report, &rows).expect("the plan cuts");
    assert!(!block.contains("No memory limit"), "{block}");
    assert!(block.starts_with("Memory plan: a total of 0.80 GiB (cgroup v2 memory.max"));
    // A total that holds everything warns nothing.
    let large = Reading {
        host_bytes: Some(64 * GIB),
        ..Reading::default()
    };
    let report = super::report(None, &large, &rows, &config, Baseline::Estimated);
    assert_eq!(warn_block(&report, &rows), None);
}

/// The log block's plan line names the total and its source, the reserves
/// and what the caches share; without a total it asks for one.
#[test]
fn the_plan_line_names_the_total_and_the_reserves() {
    let config = collections(RADAR);
    let rows = vec![row(
        "metatile_cache",
        Scope::Process,
        SizeSource::Default,
        1024 * MIB,
    )];
    let reading = Reading {
        limit: limit(2, "memory.max", 4),
        low_bytes: Some(GIB),
        ..Reading::default()
    };
    let report = report(None, &reading, &rows, &config, Baseline::Estimated);
    let line = header_line(&report);
    assert!(
        line.starts_with(
            "memory total 3.20 GiB (cgroup v2 memory.max 4.00 GiB × 0.8): 0.27 GiB outside the \
             caches (estimated), render budget 0.00 GiB, read budget 0.00 GiB, slack "
        ),
        "{line}"
    );
    assert!(
        line.ends_with("; cgroup memory.low 1.00 GiB (not a limit). A report: no size is changed"),
        "{line}"
    );
    let unknown = super::report(
        None,
        &Reading::default(),
        &rows,
        &config,
        Baseline::Estimated,
    );
    assert_eq!(unknown.plan, None);
    assert_eq!(
        header_line(&unknown),
        "memory total unknown: set [memory] total_mb for a plan"
    );
    assert_eq!(header_json(&unknown), Value::Null);
    // The measured baseline: resident memory less jemalloc's.
    let memory = MemoryReading(vec![
        ("process_resident_memory_bytes", Some(3 * GIB)),
        ("jemalloc_resident_bytes", Some(2 * GIB)),
    ]);
    assert!(matches!(
        baseline(&memory),
        Baseline::Measured { outside_bytes, .. } if outside_bytes == GIB
    ));
    assert_eq!(baseline(&MemoryReading::default()), Baseline::Estimated);
}
