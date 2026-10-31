use super::*;
use crate::plan::{CacheDemand, Scope};

const MB: u64 = MIB;

fn cache(id: &'static str, scope: Scope, source: SizeSource, mb: u64) -> CacheInput {
    CacheInput {
        entry: Entry::cache(id, scope, source),
        capacity_bytes: mb * MB,
        used_bytes: 0,
        entries: Some(0),
        demand: None,
        idle: false,
        unreachable: None,
    }
}

/// A cache at its built-in default of `mb`.
fn default(id: &'static str, scope: Scope, mb: u64) -> CacheInput {
    cache(id, scope, SizeSource::Default, mb)
}

fn budget(id: &'static str, scope: Scope, source: SizeSource, mb: u64) -> BudgetInput {
    BudgetInput {
        entry: Entry::budget(id, scope, source),
        capacity_bytes: mb * MB,
        unreachable: None,
    }
}

fn engine(engine: &'static str) -> Scope {
    Scope::Engine(engine)
}

fn collection(engine: &'static str, id: &str) -> Scope {
    Scope::Collection {
        engine,
        id: id.into(),
    }
}

fn source(engine: &'static str, id: &str) -> Scope {
    Scope::Source {
        engine,
        id: id.into(),
    }
}

/// A demand of `mb` from one collection, its entries of `entry_mb`; a
/// floor when `floor`.
fn demand(mb: u64, entry_mb: u64, floor: bool) -> Demand {
    let part = if floor {
        CacheDemand::floor(mb * MB, "rule", "setting")
    } else {
        CacheDemand::demand(mb * MB, "rule", "setting")
    }
    .input("field_bytes", entry_mb * MB);
    Demand {
        bytes: part.bytes,
        rule: part.rule,
        floor: part.floor,
        ahead: part.ahead,
        setting: part.setting,
        parts: vec![("c".into(), part)],
    }
}

fn with_demand(mut input: CacheInput, mb: u64, entry_mb: u64) -> CacheInput {
    input.demand = Some(demand(mb, entry_mb, false));
    input
}

fn unreachable(mut input: CacheInput) -> CacheInput {
    input.unreachable = Some("no collection".into());
    input
}

fn grant<'a>(plan: &'a Plan, id: &str, collection: Option<&str>) -> &'a Grant {
    plan.caches
        .iter()
        .chain(&plan.budgets)
        .find(|g| g.entry.id == id && g.entry.scope.collection() == collection)
        .unwrap_or_else(|| panic!("no grant for {id} {collection:?}"))
}

fn mb(plan: &Plan, id: &str) -> u64 {
    grant(plan, id, None).bytes / MB
}

/// The plan fits: its caches' expected use is within the pool, and the
/// pool, reserves and slack within the total.
fn assert_fits(plan: &Plan) {
    assert!(
        plan.expected_bytes <= plan.pool_bytes,
        "expected {} MiB over the pool of {} MiB",
        plan.expected_bytes / MB,
        plan.pool_bytes / MB
    );
    assert_eq!(plan.shortfall_bytes, 0, "{plan:#?}");
    let r = &plan.reserves;
    let reserved = r.baseline_bytes() + r.render_bytes + r.read_bytes + r.slack_bytes;
    assert!(reserved + plan.pool_bytes <= plan.total_bytes);
    for g in plan.caches.iter().chain(&plan.budgets) {
        assert!(g.expected_bytes <= g.bytes, "{g:?}");
        assert!(
            g.bytes >= g.floor_bytes || g.reason == Reason::Pinned,
            "{g:?}"
        );
    }
}

/// No cache a collection can reach is below its size today, unless pinned
/// (and so at it).
fn assert_none_below_today(plan: &Plan) {
    for g in &plan.caches {
        if matches!(g.reason, Reason::Unreachable(_)) {
            continue;
        }
        assert!(
            g.bytes >= g.capacity_bytes,
            "{} {:?} at {} MiB, below its {} MiB today ({})",
            g.entry.id,
            g.entry.scope.collection(),
            g.bytes / MB,
            g.capacity_bytes / MB,
            g.reason
        );
    }
    assert!(!plan.cut);
}

/// Every process-wide cache and budget at its built-in default, as a
/// fresh process enters them; those of engines in `absent` unreachable.
fn process_wide(absent: &[&str]) -> (Vec<CacheInput>, Vec<BudgetInput>) {
    let caches = vec![
        default("metatile_cache", Scope::Process, 1024),
        default("rendered_cache", Scope::Process, 256),
        cache(
            "vector_tile_cache",
            Scope::Process,
            SizeSource::Constant,
            128,
        ),
        cache(
            "solid_image_cache",
            Scope::Process,
            SizeSource::Constant,
            32,
        ),
        default("station_frame_cache", Scope::Process, 32),
        default("tiles3d_content_cache", Scope::Process, 512),
        default("pvol_pixel_cache", engine("odim-volume"), 1024),
        default("pvol_voxel_grid_cache", engine("odim-volume"), 512),
        default("pvol_cell_set_cache", engine("odim-volume"), 64),
        default("odim_composite_cache", engine("odim"), 2048),
        default("geotiff_decoded_chunk_cache", engine("geotiff"), 512),
        default("satellite_frame_cache", engine("satellite"), 1024),
        default("satellite_strip_cache", engine("satellite"), 256),
        default("netcdf_field_cache", engine("netcdf"), 256),
        default("netcdf_chunk_cache", engine("netcdf"), 512),
        default("netcdf_block_cache", engine("netcdf"), 64),
        default("station_grid_cache", engine("station-grid"), 256),
    ];
    let budgets = vec![
        budget("render_budget", Scope::Process, SizeSource::Default, 1024),
        budget("edr_response", Scope::Process, SizeSource::Default, 512),
        budget(
            "geotiff_decode",
            engine("geotiff"),
            SizeSource::Default,
            1024,
        ),
        budget("zarr_read", engine("zarr"), SizeSource::Default, 1024),
        budget("netcdf_read", engine("netcdf"), SizeSource::Default, 1024),
        budget(
            "pvol_read",
            engine("odim-volume"),
            SizeSource::Default,
            1024,
        ),
        budget(
            "station_grid_compute",
            engine("station-grid"),
            SizeSource::Default,
            512,
        ),
    ];
    let reach = |id: &str, scope: &Scope| -> bool {
        let absent = |name: &str| absent.contains(&name);
        match (id, scope) {
            ("tiles3d_content_cache" | "pvol_voxel_grid_cache", _) => !absent("3dtiles"),
            ("vector_tile_cache", _) => !absent("mvt"),
            ("station_frame_cache", _) => !absent("csv"),
            ("metatile_cache" | "rendered_cache" | "render_budget", _) => !absent("wms"),
            ("edr_response", _) => !absent("edr"),
            (_, Scope::Engine(engine)) => !absent(engine),
            _ => true,
        }
    };
    let caches = caches
        .into_iter()
        .map(|c| {
            if reach(c.entry.id, &c.entry.scope) {
                c
            } else {
                unreachable(c)
            }
        })
        .collect();
    let budgets = budgets
        .into_iter()
        .map(|mut b| {
            if !reach(b.entry.id, &b.entry.scope) {
                b.unreachable = Some("no collection".into());
            }
            b
        })
        .collect();
    (caches, budgets)
}

/// A GRIB source's three caches: a grid cache at 256 MiB with a demand of
/// 16 fields of `field_mb`, no message cache, and its point-index cache.
fn grib(id: &str, field_mb: u64) -> Vec<CacheInput> {
    vec![
        with_demand(
            default("grid_cache", source("grib", id), 256),
            16 * field_mb,
            field_mb,
        ),
        with_demand(default("grib_message_cache", source("grib", id), 0), 32, 2),
        default("grib_point_index_cache", source("grib", id), 16),
    ]
}

fn inputs(
    total_gib: f64,
    collections: u64,
    baseline: Baseline,
    caches: Vec<CacheInput>,
    budgets: Vec<BudgetInput>,
) -> Inputs {
    Inputs {
        total_bytes: (total_gib * GIB as f64) as u64,
        collections,
        baseline,
        caches,
        budgets,
    }
}

/// The issue's small VM: 2 GiB, one local radar, a total of 1.0 GiB. The
/// render budget is 128 MiB, the read budgets 64 MiB, the pool about half
/// the total, and the caches their floors plus a share instead of their
/// 1 GiB defaults.
#[test]
fn a_small_vm_plans_floors_plus_a_share() {
    let (mut caches, budgets) = process_wide(&[
        "3dtiles",
        "mvt",
        "csv",
        "edr",
        "odim",
        "odim-volume",
        "satellite",
        "netcdf",
        "station-grid",
        "zarr",
    ]);
    caches.push(unreachable(cache(
        "tile_cache",
        collection("geotiff", "radar"),
        SizeSource::Constant,
        0,
    )));
    let plan = plan(&inputs(1.0, 1, Baseline::Estimated, caches, budgets));
    assert_fits(&plan);
    assert_eq!(plan.reserves.render_bytes, 128 * MB);
    assert_eq!(plan.reserves.read_bytes, 64 * MB);
    assert_eq!(plan.reserves.read_budget, Some("geotiff_decode"));
    assert_eq!(plan.reserves.baseline_bytes(), (256 + 16) * MB);
    assert_eq!(plan.pool_bytes, 509 * MB);
    assert!(plan.cut, "1 GiB of defaults does not fit");
    // The traffic-driven caches share the pool by weight above their
    // floors: the meta-tile cache most, and none at its default.
    let (metatile, rendered, decoded) = (
        mb(&plan, "metatile_cache"),
        mb(&plan, "rendered_cache"),
        mb(&plan, "geotiff_decoded_chunk_cache"),
    );
    assert!(metatile > decoded && decoded > rendered, "{plan:#?}");
    assert!((128..1024).contains(&metatile), "{metatile}");
    assert!((32..256).contains(&rendered), "{rendered}");
    assert!((64..512).contains(&decoded), "{decoded}");
    assert_eq!(
        grant(&plan, "metatile_cache", None).reason,
        Reason::Cut(Step::Traffic)
    );
    // Within 1 MiB a cache of the pool, after the solid-image memo.
    let shared = metatile + rendered + decoded;
    assert!((509 - 32 - 3..=509 - 32).contains(&shared), "{shared}");
    assert_eq!(mb(&plan, "solid_image_cache"), 32);
    assert_eq!(
        grant(&plan, "solid_image_cache", None).reason,
        Reason::Fixed
    );
    assert_eq!(mb(&plan, "odim_composite_cache"), 0);
    assert_eq!(mb(&plan, "zarr_read"), 0);
}

/// The issue's container: a 4 GiB limit, ten collections, a total of 3.2
/// GiB. Render budget 0.32 GiB, read budgets 0.16 GiB, a pool of 2.1 GiB,
/// and unreachable caches at 0.
#[test]
fn a_4_gib_container_fits_its_caches_into_the_pool() {
    let (mut caches, budgets) = process_wide(&["3dtiles", "mvt", "csv", "odim", "zarr"]);
    caches.extend(grib("global", 4));
    caches.extend(grib("regional", 25));
    caches.push(with_demand(
        default("satellite_frame_cache", engine("satellite"), 1024),
        650,
        30,
    ));
    caches.retain(|c| !(c.entry.id == "satellite_frame_cache" && c.demand.is_none()));
    let plan = plan(&inputs(3.2, 10, Baseline::Estimated, caches, budgets));
    assert_fits(&plan);
    assert_eq!(plan.reserves.render_bytes, 327 * MB);
    assert_eq!(plan.reserves.read_bytes, 163 * MB);
    assert_eq!(plan.reserves.baseline_bytes(), (256 + 160) * MB);
    let pool_gib = plan.pool_bytes as f64 / GIB as f64;
    assert!((2.0..2.2).contains(&pool_gib), "{pool_gib}");
    for id in [
        "tiles3d_content_cache",
        "pvol_voxel_grid_cache",
        "odim_composite_cache",
    ] {
        assert_eq!(mb(&plan, id), 0, "{id}");
    }
    // The satellite scans keep their demand plus 15 % (above the 1 GiB
    // default here, which the demand alone sets).
    assert_eq!(mb(&plan, "satellite_frame_cache"), 1024);
    assert_eq!(
        grant(&plan, "satellite_frame_cache", None).expected_bytes,
        650 * MB
    );
    // A cut leaves no sliver: a working set below 64 MiB or four of its
    // fields goes to its floor, a message cache off today back to 0.
    assert!(plan.cut);
    let regional = grant(&plan, "grid_cache", Some("regional"));
    assert!(regional.bytes >= 100 * MB, "{regional:?}");
    assert_eq!(grant(&plan, "grib_message_cache", Some("global")).bytes, 0);
}

/// The issue's laptop: 16 GiB with 6 GiB available, five collections, a
/// total of min(8, 4.8) = 4.8 GiB and a pool of 3.4 GiB.
#[test]
fn a_laptop_fits_its_caches_into_the_pool() {
    let (mut caches, budgets) = process_wide(&[
        "3dtiles",
        "mvt",
        "csv",
        "odim",
        "odim-volume",
        "satellite",
        "netcdf",
        "station-grid",
    ]);
    caches.extend(grib("global-a", 4));
    caches.extend(grib("global-b", 4));
    caches.push(default(
        "zarr_object_cache",
        collection("zarr", "reanalysis"),
        256,
    ));
    caches.push(unreachable(cache(
        "zarr_decoded_cache",
        collection("zarr", "reanalysis"),
        SizeSource::Constant,
        0,
    )));
    let plan = plan(&inputs(4.8, 5, Baseline::Estimated, caches, budgets));
    assert_fits(&plan);
    let pool_gib = plan.pool_bytes as f64 / GIB as f64;
    assert!((3.3..3.5).contains(&pool_gib), "{pool_gib}");
    assert_none_below_today(&plan);
}

/// A config like the issue's production one: 79 collections, measured, a
/// total of 31 GiB on a shared 62 GiB host without a limit. With `pinned`,
/// its pins: meta-tiles, PVOL pixels, decoded chunks, satellite scans, the
/// render budget and two GRIB message caches.
fn production(pinned: bool) -> Inputs {
    let (mut caches, mut budgets) = process_wide(&["3dtiles"]);
    for c in caches.iter_mut() {
        match c.entry.id {
            "pvol_pixel_cache" => c.demand = Some(demand(900, 3, true)),
            "satellite_frame_cache" => c.demand = Some(demand(1880, 30, false)),
            "netcdf_field_cache" => c.demand = Some(demand(192, 4, false)),
            "station_grid_cache" => c.demand = Some(demand(20, 1, false)),
            _ => {}
        }
        if pinned {
            let pin = match c.entry.id {
                "metatile_cache" => Some((SizeSource::Config("[server] metatile_cache_mb"), 8192)),
                "pvol_pixel_cache" => Some((SizeSource::Env("MC_PVOL_PIXEL_CACHE_MB"), 4096)),
                "geotiff_decoded_chunk_cache" => {
                    Some((SizeSource::Env("MC_GEOTIFF_DECODED_CHUNK_CACHE_MB"), 2048))
                }
                "satellite_frame_cache" => {
                    Some((SizeSource::Env("MC_SATELLITE_FRAME_CACHE_MB"), 3072))
                }
                _ => None,
            };
            if let Some((source, mb)) = pin {
                c.entry.source = source;
                c.capacity_bytes = mb * MB;
            }
        }
    }
    if pinned {
        budgets[0].entry.source = SizeSource::Env("MC_RENDER_MEMORY_MB");
        budgets[0].capacity_bytes = 4096 * MB;
    }
    caches.push(default("lightning_strike_cache", engine("postgis"), 32));
    for (id, field_mb) in [
        ("global-a", 4),
        ("global-b", 4),
        ("global-c", 4),
        ("global-d", 4),
        ("regional-a", 2),
        ("regional-b", 25),
    ] {
        let mut source_caches = grib(id, field_mb);
        if pinned && ["global-a", "global-b"].contains(&id) {
            source_caches[1].entry.source = SizeSource::Collection("[grib] message_cache_mb");
            source_caches[1].capacity_bytes = 1024 * MB;
            source_caches[2].entry.source = SizeSource::Derived("[grib] message_cache_mb");
            source_caches[2].capacity_bytes = 256 * MB;
        }
        caches.extend(source_caches);
    }
    for id in ["reanalysis-a", "reanalysis-b"] {
        caches.push(default("zarr_object_cache", collection("zarr", id), 256));
    }
    caches.push(with_demand(
        default("tile_cache", collection("geotiff", "mosaic"), 256),
        200,
        1,
    ));
    for id in ["radar-a", "radar-b", "radar-c"] {
        caches.push(unreachable(cache(
            "tile_cache",
            collection("geotiff", id),
            SizeSource::Constant,
            0,
        )));
    }
    inputs(
        31.0,
        79,
        Baseline::Measured {
            outside_bytes: 655 * MB,
            store_bytes: 200 * MB,
        },
        caches,
        budgets,
    )
}

/// Nothing pinned at 31 GiB: every cache a collection reaches keeps at
/// least its size today, and the traffic-driven ones share the rest.
#[test]
fn production_at_31_gib_keeps_every_size_and_shares_the_rest() {
    let plan = plan(&production(false));
    assert_fits(&plan);
    assert_none_below_today(&plan);
    let pool_gib = plan.pool_bytes as f64 / GIB as f64;
    assert!((19.0..23.0).contains(&pool_gib), "{pool_gib}");
    // The traffic-driven caches share what the others leave by weight.
    let metatile = mb(&plan, "metatile_cache");
    let pixels = mb(&plan, "pvol_pixel_cache");
    let decoded = mb(&plan, "geotiff_decoded_chunk_cache");
    assert!(metatile > pixels && pixels > decoded, "{plan:#?}");
    assert_eq!(
        grant(&plan, "metatile_cache", None).reason,
        Reason::Share(45)
    );
    // Its grant would be shaped for 3 MiB sweeps.
    assert_eq!(
        grant(&plan, "pvol_pixel_cache", None).entry_bytes,
        Some(3 * MB)
    );
    // The 0.1° source's grid cache holds sixteen of its 25 MiB fields; a
    // 0.25° one keeps its size today.
    let regional = grant(&plan, "grid_cache", Some("regional-b"));
    assert_eq!(regional.bytes, 460 * MB);
    assert_eq!(regional.reason, Reason::WorkingSet);
    assert_eq!(regional.expected_bytes, 400 * MB);
    assert_eq!(grant(&plan, "grid_cache", Some("global-c")).bytes, 256 * MB);
    assert_eq!(
        grant(&plan, "grid_cache", Some("global-c")).reason,
        Reason::Today
    );
    // A remote source's message cache, off today, would get 64 MiB.
    assert_eq!(
        grant(&plan, "grib_message_cache", Some("global-c")).bytes,
        64 * MB
    );
    assert_eq!(mb(&plan, "tiles3d_content_cache"), 0);
}

/// The same with production's pins, a 4 GiB render budget among them:
/// they keep their sizes, and what they leave is still planned.
#[test]
fn production_s_pins_keep_their_sizes() {
    let inputs = production(true);
    let plan = plan(&inputs);
    for input in inputs.caches.iter().filter(|c| pinned(&c.entry.source)) {
        let g = plan.grant(&input.entry).unwrap();
        assert_eq!(g.bytes, input.capacity_bytes, "{g:?}");
        assert_eq!(g.reason, Reason::Pinned);
    }
    assert_eq!(mb(&plan, "render_budget"), 4096);
    assert_eq!(
        plan.pinned_bytes,
        (8192 + 4096 + 2048 + 3072 + 2 * 1024 + 2 * 256) * MB
    );
    // What the pins leave does not hold the composite cache at its size
    // today: it is cut, and the rest fits.
    assert_fits(&plan);
    let composite = grant(&plan, "odim_composite_cache", None);
    assert_eq!(composite.reason, Reason::Cut(Step::Traffic));
    assert!(composite.bytes < 2048 * MB);
}

/// The issue's dedicated server: 128 GiB, a total of 64 GiB. Budgets at
/// their ceilings, a pool of about 53 GiB, and every size kept.
#[test]
fn a_dedicated_server_at_64_gib_keeps_every_size() {
    let mut inputs = production(false);
    inputs.total_bytes = 64 * GIB;
    inputs.collections = 40;
    inputs.baseline = Baseline::Estimated;
    let plan = plan(&inputs);
    assert_fits(&plan);
    assert_none_below_today(&plan);
    assert_eq!(plan.reserves.render_bytes, 4 * GIB);
    assert_eq!(plan.reserves.read_bytes, GIB);
    let pool_gib = plan.pool_bytes as f64 / GIB as f64;
    assert!((50.0..54.0).contains(&pool_gib), "{pool_gib}");
    // No traffic-driven cache past eight times its size today.
    assert_eq!(mb(&plan, "rendered_cache"), 2048);
    assert!(mb(&plan, "metatile_cache") <= 8192);
}

/// A small config of every class: a pinned meta-tile cache, a demand cache,
/// a working-set cache, a traffic-driven one, a fixed one, an unreachable
/// one; the render budget pinned, a read budget scaled.
fn mixed(total_gib: f64) -> Inputs {
    let caches = vec![
        cache(
            "metatile_cache",
            Scope::Process,
            SizeSource::Config("[server] metatile_cache_mb"),
            2048,
        ),
        with_demand(
            default("satellite_frame_cache", engine("satellite"), 1024),
            300,
            30,
        ),
        with_demand(default("grid_cache", source("grib", "model"), 256), 400, 25),
        default("rendered_cache", Scope::Process, 256),
        cache(
            "solid_image_cache",
            Scope::Process,
            SizeSource::Constant,
            32,
        ),
        unreachable(default("tiles3d_content_cache", Scope::Process, 512)),
        cache(
            "grib_point_index_cache",
            source("grib", "model"),
            SizeSource::Derived("[grib] message_cache_mb"),
            256,
        ),
    ];
    let budgets = vec![
        budget(
            "render_budget",
            Scope::Process,
            SizeSource::Env("MC_RENDER_MEMORY_MB"),
            2048,
        ),
        budget(
            "pvol_read",
            engine("odim-volume"),
            SizeSource::Default,
            1024,
        ),
        BudgetInput {
            unreachable: Some("no zarr collection".into()),
            ..budget("zarr_read", engine("zarr"), SizeSource::Default, 1024)
        },
    ];
    inputs(total_gib, 3, Baseline::Estimated, caches, budgets)
}

/// #1312 acceptance: pinned sizes are never changed, at any total.
#[test]
fn pinned_sizes_are_never_changed() {
    for total in [0.25, 0.5, 1.0, 2.0, 4.0, 64.0] {
        let plan = plan(&mixed(total));
        for (id, mb) in [
            ("metatile_cache", 2048),
            ("grib_point_index_cache", 256),
            ("render_budget", 2048),
        ] {
            let g = plan
                .caches
                .iter()
                .chain(&plan.budgets)
                .find(|g| g.entry.id == id)
                .unwrap();
            assert_eq!(
                (g.bytes, &g.reason),
                (mb * MB, &Reason::Pinned),
                "{id} at {total} GiB"
            );
        }
    }
}

/// The render budget scales up to 4 GiB; a read budget never above its
/// size today, so the 512 MiB ones stay there on a large host.
#[test]
fn read_budgets_stay_at_most_their_size_today() {
    let (_, budgets) = process_wide(&[]);
    let plan = plan(&inputs(64.0, 10, Baseline::Estimated, Vec::new(), budgets));
    assert_eq!(mb(&plan, "render_budget"), 4096);
    for (id, mb_today) in [
        ("edr_response", 512),
        ("station_grid_compute", 512),
        ("pvol_read", 1024),
        ("zarr_read", 1024),
    ] {
        assert_eq!(mb(&plan, id), mb_today, "{id}");
    }
    assert_eq!(plan.reserves.read_bytes, GIB);
    assert_eq!(plan.reserves.read_budget, Some("geotiff_decode"));
    // Below its size today, 5 % of the total.
    let (_, budgets) = process_wide(&[]);
    let small = super::plan(&inputs(4.0, 10, Baseline::Estimated, Vec::new(), budgets));
    assert_eq!(mb(&small, "edr_response"), 204);
}

/// #1312 acceptance: a cache or budget no collection can reach gets 0, and
/// an unreachable budget is not reserved.
#[test]
fn absent_engines_get_0() {
    let plan = plan(&mixed(8.0));
    let g = grant(&plan, "tiles3d_content_cache", None);
    assert_eq!((g.bytes, g.expected_bytes), (0, 0));
    assert_eq!(g.reason.to_string(), "unreachable: no collection");
    assert_eq!(mb(&plan, "zarr_read"), 0);
    assert_eq!(plan.reserves.read_budget, Some("pvol_read"));
}

/// #1312 acceptance: a total far below the pins is a shortfall, never a
/// failure: the pins stay, every other cache and budget goes to its floor,
/// and the shortfall is what the total lacks.
#[test]
fn overcommit_is_a_shortfall_never_a_failure() {
    let plan = plan(&mixed(2.0));
    assert!(plan.cut);
    assert!(plan.shortfall_bytes > 0);
    assert_eq!(mb(&plan, "metatile_cache"), 2048);
    assert_eq!(mb(&plan, "rendered_cache"), 32, "its floor");
    assert_eq!(mb(&plan, "pvol_read"), 64, "its floor");
    assert_eq!(
        grant(&plan, "satellite_frame_cache", None).reason,
        Reason::Cut(Step::Demand)
    );
    // Four of its 30 MiB scans.
    assert_eq!(mb(&plan, "satellite_frame_cache"), 120);
    let r = &plan.reserves;
    let needed = r.baseline_bytes()
        + r.render_bytes
        + r.read_bytes
        + plan.expected_bytes
        + plan.expected_bytes / 10;
    assert!(needed.abs_diff(plan.total_bytes + plan.shortfall_bytes) <= MB);
}

/// #1312 acceptance: the same caches and budgets in another order give the
/// same plan.
#[test]
fn the_same_config_in_any_order_gives_the_same_plan() {
    for inputs in [mixed(1.0), mixed(3.0), production(false), production(true)] {
        let mut reversed = inputs.clone();
        reversed.caches.reverse();
        reversed.budgets.reverse();
        let mut rotated = inputs.clone();
        rotated.caches.rotate_left(3);
        rotated.budgets.rotate_left(1);
        let expected = plan(&inputs);
        assert_eq!(plan(&reversed), expected);
        assert_eq!(plan(&rotated), expected);
    }
}

/// Rules 3 and 6: a demand cache gets its demand plus 15 %, its size today
/// when that is more; its expected use is its demand.
#[test]
fn a_demand_cache_gets_its_demand_plus_15_percent() {
    let at = |demand_mb| {
        let mut inputs = mixed(16.0);
        inputs.caches[1].demand = Some(demand(demand_mb, 30, false));
        plan(&inputs)
    };
    let large = at(2000);
    let g = grant(&large, "satellite_frame_cache", None);
    assert_eq!((g.bytes, g.expected_bytes), (2300 * MB, 2000 * MB));
    assert_eq!(g.reason, Reason::Demand);
    let small = at(300);
    let g = grant(&small, "satellite_frame_cache", None);
    assert_eq!((g.bytes, g.expected_bytes), (1024 * MB, 300 * MB));
    assert_eq!(g.reason, Reason::Today);
    assert_eq!(g.floor_bytes, 120 * MB, "four scans");
}

/// Rule 4: the working sets together get at most 35 % of what the pinned,
/// fixed and demand caches leave; rule 6 raises each back to its size
/// today while that fits.
#[test]
fn working_sets_share_at_most_35_percent() {
    let ws = |id: &str, mb| with_demand(default("grid_cache", source("grib", id), 0), mb, 25);
    let caches = vec![ws("a", 1000), ws("b", 3000)];
    let plan = plan(&inputs(8.0, 2, Baseline::Estimated, caches, Vec::new()));
    let cap = plan.pool_bytes * 35 / 100;
    let (a, b) = (
        grant(&plan, "grid_cache", Some("a")),
        grant(&plan, "grid_cache", Some("b")),
    );
    assert!(a.bytes + b.bytes <= cap, "{a:?} {b:?}");
    assert!(a.bytes + b.bytes >= cap - 2 * MB, "{a:?} {b:?}");
    // 1150 and 2048 (the ceiling) scaled alike.
    let ratio = b.bytes as f64 / a.bytes as f64;
    assert!((ratio - 2048.0 / 1150.0).abs() < 0.01, "{ratio}");
}

/// Rule 5: the traffic-driven caches share the rest by weight, each from
/// its size today to eight times it; the PVOL pixel cache's weight sits on
/// its pre-warm floor.
fn plan_caches() -> Vec<CacheInput> {
    vec![
        default("metatile_cache", Scope::Process, 1024),
        default("rendered_cache", Scope::Process, 256),
        CacheInput {
            demand: Some(demand(2000, 3, true)),
            ..default("pvol_pixel_cache", engine("odim-volume"), 1024)
        },
    ]
}

#[test]
fn traffic_driven_caches_share_by_weight() {
    let plan = plan(&inputs(
        16.0,
        1,
        Baseline::Estimated,
        plan_caches(),
        Vec::new(),
    ));
    let (metatile, rendered, pixels) = (
        mb(&plan, "metatile_cache"),
        mb(&plan, "rendered_cache"),
        mb(&plan, "pvol_pixel_cache"),
    );
    let pool = plan.pool_bytes / MB;
    assert!(metatile + rendered + pixels <= pool);
    assert!(
        metatile + rendered + pixels + 3 >= pool,
        "the pool is shared"
    );
    // 45 : 5, and 45 : 25 above the 2000 MiB floor.
    let ratio = metatile as f64 / rendered as f64;
    assert!((ratio - 9.0).abs() < 0.05, "{ratio}");
    let ratio = metatile as f64 / (pixels - 2000) as f64;
    assert!((ratio - 45.0 / 25.0).abs() < 0.01, "{ratio}");
    // A smaller share, then each at its ceiling: eight times today.
    let mut inputs = inputs(64.0, 1, Baseline::Estimated, plan_caches(), Vec::new());
    inputs.caches.truncate(2);
    let large = super::plan(&inputs);
    assert_eq!(mb(&large, "metatile_cache"), 8192);
    assert_eq!(mb(&large, "rendered_cache"), 2048);
}

/// Rule 7: a cut takes idle caches first, then the traffic-driven ones,
/// the working sets, the budgets, and the demand caches last.
#[test]
fn cuts_follow_the_order() {
    let caches = || {
        vec![
            CacheInput {
                idle: true,
                ..default("rendered_cache", Scope::Process, 1024)
            },
            default("metatile_cache", Scope::Process, 1024),
            default("grid_cache", source("grib", "a"), 1024),
            with_demand(
                default("satellite_frame_cache", engine("satellite"), 0),
                1024,
                16,
            ),
        ]
    };
    let budgets = || {
        vec![budget(
            "render_budget",
            Scope::Process,
            SizeSource::Default,
            1024,
        )]
    };
    let cut = |total: f64| -> [bool; 5] {
        let plan = plan(&inputs(total, 1, Baseline::Estimated, caches(), budgets()));
        assert_fits(&plan);
        [
            ("rendered_cache", None),
            ("metatile_cache", None),
            ("grid_cache", Some("a")),
            ("render_budget", None),
            ("satellite_frame_cache", None),
        ]
        .map(|(id, collection)| matches!(grant(&plan, id, collection).reason, Reason::Cut(_)))
    };
    assert_eq!(cut(5.5), [false; 5], "fits");
    assert_eq!(cut(5.0), [true, false, false, false, false]);
    assert_eq!(cut(3.6), [true, true, false, false, false]);
    assert_eq!(cut(2.5), [true, true, true, false, false]);
    assert_eq!(cut(1.8), [true; 5]);
    let plan = plan(&inputs(1.8, 1, Baseline::Estimated, caches(), budgets()));
    assert_eq!(
        grant(&plan, "satellite_frame_cache", None).reason,
        Reason::Cut(Step::Demand)
    );
    assert_eq!(mb(&plan, "render_budget"), 128, "its floor");
}

/// Under 512 MiB every cache and budget the plan sizes is at its floor.
#[test]
fn under_512_mib_everything_is_at_its_floor() {
    let plan = plan(&mixed(0.25));
    assert!(plan.shortfall_bytes > 0);
    for (id, floor) in [
        ("rendered_cache", 32),
        // Four of its 25 MiB fields.
        ("grid_cache", 100),
        ("satellite_frame_cache", 120),
        ("pvol_read", 64),
    ] {
        let g = plan
            .caches
            .iter()
            .chain(&plan.budgets)
            .find(|g| g.entry.id == id)
            .unwrap();
        assert_eq!((g.bytes / MB, &g.reason), (floor, &Reason::Floor), "{id}");
    }
}

/// Rule 8: a grant carries the size of the cache's entries: what its
/// engines publish, else the mean of what it holds.
#[test]
fn a_grant_carries_the_entry_size() {
    let plan = plan(&mixed(16.0));
    assert_eq!(
        grant(&plan, "grid_cache", Some("model")).entry_bytes,
        Some(25 * MB)
    );
    let mut held = default("rendered_cache", Scope::Process, 256);
    held.used_bytes = 30 * MB;
    held.entries = Some(10);
    assert_eq!(held.entry_bytes(), Some(3 * MB));
    held.entries = Some(0);
    assert_eq!(held.entry_bytes(), None);
}

#[test]
fn reasons_print_for_the_report() {
    let printed: Vec<String> = [
        Reason::Pinned,
        Reason::Unreachable("no 3dtiles collection".into()),
        Reason::Fixed,
        Reason::Demand,
        Reason::WorkingSet,
        Reason::Share(45),
        Reason::Today,
        Reason::Scaled,
        Reason::Cut(Step::Traffic),
        Reason::Cut(Step::Demand),
        Reason::Floor,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        printed,
        [
            "pinned",
            "unreachable: no 3dtiles collection",
            "fixed",
            "demand + 15 %",
            "working set",
            "share by weight 45",
            "size today",
            "scaled to the total",
            "cut: traffic",
            "cut: below its demand",
            "floor: total under 512 MiB",
        ]
    );
}
