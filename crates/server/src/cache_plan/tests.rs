use std::collections::BTreeSet;
use std::path::Path;

use axum::response::IntoResponse;

use super::*;

fn testdata(path: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(path)
}

/// A config of one collection of every engine type with a cache or budget
/// of its own that loads from committed fixtures, written to `dir`: a local
/// GeoTIFF radar, two GRIB sources (one with `grid_cache_mb = 512`), Zarr,
/// NetCDF (an empty directory), a PVOL with 3D Tiles, an ODIM composite,
/// a satellite collection and a station grid over the CSV stations.
fn every_engine_config(dir: &Path) -> std::path::PathBuf {
    let pvol = dir.join("pvol");
    std::fs::create_dir(&pvol).unwrap();
    std::fs::copy(testdata("pvol-cold-batch.h5"), pvol.join("pvol.h5")).unwrap();
    let comp = dir.join("comp");
    std::fs::create_dir(&comp).unwrap();
    std::fs::copy(
        testdata("odim-dmi-fixture.h5"),
        comp.join("dk.com.202601201125.500_max.h5"),
    )
    .unwrap();
    let goes = dir.join("goes");
    let hour = goes.join("ABI-L2/2026/268/19");
    std::fs::create_dir_all(&hour).unwrap();
    let c13 = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
    std::fs::copy(testdata("goes19-abi").join(c13), hour.join(c13)).unwrap();
    let netcdf = dir.join("netcdf");
    std::fs::create_dir(&netcdf).unwrap();
    let path = |p: &Path| p.to_str().unwrap().to_string();
    let config = format!(
        r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "radar"
title = "Radar"
description = "test"
engine_type = "geotiff"
data_path = {radar:?}
apis = ["wms", "maps", "tiles", "edr"]
[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"

[[collections]]
id = "grib-default"
title = "GRIB"
description = "test"
engine_type = "grib"
apis = ["edr", "wms"]
[collections.grib]
data_path = {grib:?}

[[collections]]
id = "grib-sized"
title = "GRIB"
description = "test"
engine_type = "grib"
apis = ["edr", "wms"]
[collections.grib]
data_path = {grib:?}
grid_cache_mb = 512

[[collections]]
id = "zarr"
title = "Zarr"
description = "test"
engine_type = "zarr"
apis = ["edr", "wms"]
[collections.zarr]
data_path = {zarr:?}

[[collections]]
id = "netcdf"
title = "NetCDF"
description = "test"
engine_type = "netcdf"
apis = ["wms"]
[collections.netcdf]
data_path = {netcdf:?}

[[collections]]
id = "pvol"
title = "Volumes"
description = "test"
engine_type = "odim-volume"
apis = ["edr", "wms", "3dtiles"]
data_path = {pvol:?}
[collections.odim]
prewarm_sweeps = 0

[[collections]]
id = "comp"
title = "Composite"
description = "test"
engine_type = "odim"
apis = ["wms", "edr"]
data_path = {comp:?}
[collections.odim]
filename_template = "dk.com.%Y%m%d%H%M.500_max.h5"
parameter = "DBZH"
unit = "dBZ"

[[collections]]
id = "goes"
title = "GOES"
description = "test"
engine_type = "satellite"
apis = ["wms"]
[collections.satellite]
data_path = {goes:?}
[[collections.satellite.products]]
parameter = "ir_10_3"
title = "IR 10.3"
unit = "K"
product = "ABI-L2-CMIPF"
band = 13
variable = "CMI"

[[collections]]
id = "weather"
title = "Weather"
description = "Stations"
engine_type = "csv"
data_path = {weather:?}
apis = ["features"]

[[collections]]
id = "grid"
title = "Grid"
description = "Analysed"
engine_type = "station-grid"
apis = ["wms"]
[collections.station_grid]
source = "weather"
parameters = ["temperature"]
grid = {{ bbox = [21.0, 60.0, 23.0, 62.0], resolution = 0.05 }}
search_radius_km = 60
cadence = "PT1H"
precompute = 1
"#,
        radar = path(&testdata("radar")),
        grib = path(&testdata("grib-local")),
        zarr = path(&testdata("zarr-era5-t2m")),
        netcdf = path(&netcdf),
        pvol = path(&pvol),
        comp = path(&comp),
        goes = path(&goes),
        weather = path(&testdata("weather.csv")),
    );
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config).unwrap();
    config_path
}

/// The series of a `/metrics` gauge: its `collection` label and value.
fn series(metrics: &str, family: &str) -> Vec<(Option<String>, u64)> {
    metrics
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(family)?;
            let (labels, value) = match rest.strip_prefix('{') {
                Some(rest) => rest.split_once("} ")?,
                None => ("", rest.strip_prefix(' ')?),
            };
            let collection = labels
                .split(',')
                .find_map(|label| label.strip_prefix("collection=\""))
                .map(|v| v.trim_end_matches('"').to_string());
            Some((collection, value.parse().ok()?))
        })
        .collect()
}

/// Every gauge family of `metrics` whose name ends with `suffix`.
fn families<'a>(metrics: &'a str, suffix: &str) -> BTreeSet<&'a str> {
    metrics
        .lines()
        .filter_map(|line| line.strip_prefix("# TYPE ")?.strip_suffix(" gauge"))
        .filter(|name| name.ends_with(suffix))
        .collect()
}

/// #1306 acceptance: every cache family and every admission budget on
/// `/metrics` has a plan entry, with the capacity `/metrics` reports, and
/// its source; an environment variable and a collection's key show as
/// theirs; the log block and the endpoint list the same sizes. A new cache
/// with a `*_cache_bytes` family but no plan entry fails here.
///
/// Runs in a child process: the process-wide caches read their sizes from
/// the environment once, and `MC_PVOL_PIXEL_CACHE_MB` must be set first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_cache_and_budget_on_metrics_is_in_the_plan() {
    const CHILD: &str = "MC_TEST_CACHE_PLAN_CHILD";
    const NAME: &str = "cache_plan::tests::every_cache_and_budget_on_metrics_is_in_the_plan";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, "1")
            .env("MC_PVOL_PIXEL_CACHE_MB", "2048")
            .status()
            .unwrap();
        assert!(status.success(), "{NAME} failed in its child process");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let state = crate::watcher::tests::build_state(&every_engine_config(dir.path()));
    for health in state.health.read().unwrap().iter() {
        assert_ne!(
            health.status,
            crate::admin::CollectionStatus::Failed,
            "{} failed to load: {:?}",
            health.id,
            health.error
        );
    }
    log(&state, Occasion::Boot);
    let response = crate::admin::metrics_handler(State(state.clone()))
        .await
        .into_response();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let metrics = String::from_utf8(body.to_vec()).unwrap();
    let rows = snapshot(&state);
    let row = |id: &str, collection: Option<&str>| {
        rows.iter()
            .find(|r| r.entry.id == id && r.entry.scope.collection() == collection)
    };

    // Capacity families name their entry: `X_capacity_bytes` is `X`, and
    // the render budget's is `render_budget_total_bytes`.
    let mut checked = BTreeSet::new();
    let capacity_families = families(&metrics, "_capacity_bytes");
    let capacities = capacity_families
        .iter()
        .map(|family| (family.strip_suffix("_capacity_bytes").unwrap(), *family))
        .chain([("render_budget", "render_budget_total_bytes")]);
    for (id, family) in capacities {
        for (collection, capacity) in series(&metrics, family) {
            let row = row(id, collection.as_deref())
                .unwrap_or_else(|| panic!("{family} {collection:?} has no plan entry"));
            assert_eq!(
                row.usage.capacity_bytes, capacity,
                "{family} {collection:?}"
            );
            checked.insert(id);
        }
    }
    for family in families(&metrics, "_cache_bytes") {
        let id = family.strip_suffix("_bytes").unwrap();
        for (collection, _) in series(&metrics, family) {
            assert!(
                row(id, collection.as_deref()).is_some(),
                "{family} {collection:?} has no plan entry"
            );
            checked.insert(id);
        }
    }
    let expected = [
        "metatile_cache",
        "rendered_cache",
        "station_frame_cache",
        "tile_cache",
        "geotiff_decoded_chunk_cache",
        "grid_cache",
        "grib_message_cache",
        "grib_point_index_cache",
        "zarr_decoded_cache",
        "netcdf_field_cache",
        "netcdf_block_cache",
        "netcdf_chunk_cache",
        "pvol_pixel_cache",
        "pvol_voxel_grid_cache",
        "pvol_cell_set_cache",
        "tiles3d_content_cache",
        "odim_composite_cache",
        "satellite_frame_cache",
        "satellite_strip_cache",
        "station_grid_cache",
        "vector_tile_cache",
        "solid_image_cache",
        "zarr_object_cache",
        "render_budget",
        "edr_response",
        "geotiff_decode",
        "zarr_read",
        "netcdf_read",
        "pvol_read",
        "station_grid_compute",
    ];
    for id in expected {
        assert!(checked.contains(id), "{id} not on /metrics: {checked:?}");
    }

    // Every budget has a high-water mark (#1307): its `*_reserved_peak_bytes`.
    for row in rows.iter().filter(|row| row.entry.kind == Kind::Budget) {
        let family = format!("{}_reserved_peak_bytes", row.entry.id);
        assert!(
            metrics
                .lines()
                .any(|l| l.starts_with(&format!("{family} "))),
            "{family} not on /metrics"
        );
    }

    // Where the sizes came from.
    let source = |id: &str, collection: Option<&str>| {
        let row = row(id, collection).unwrap_or_else(|| panic!("{id} {collection:?}"));
        (row.entry.source.to_string(), row.usage.capacity_bytes)
    };
    assert_eq!(
        source("pvol_pixel_cache", None),
        ("env:MC_PVOL_PIXEL_CACHE_MB".into(), 2048 * MIB)
    );
    assert_eq!(
        source("grid_cache", Some("grib-sized")),
        ("collection:[grib] grid_cache_mb".into(), 512 * MIB)
    );
    assert_eq!(
        source("grid_cache", Some("grib-default")),
        ("default".into(), 256 * MIB)
    );
    assert_eq!(
        source("grib_message_cache", Some("grib-sized")),
        ("default".into(), 0)
    );
    // A local GeoTIFF reads no tiles through its tile cache.
    assert_eq!(source("tile_cache", Some("radar")), ("constant".into(), 0));
    assert_eq!(
        source("zarr_object_cache", Some("zarr")),
        ("default".into(), 256 * MIB)
    );
    assert_eq!(
        source("zarr_decoded_cache", Some("zarr")),
        ("constant".into(), 0)
    );
    assert_eq!(
        source("rendered_cache", None),
        ("default".into(), 256 * MIB)
    );
    assert_eq!(
        source("vector_tile_cache", None),
        ("constant".into(), 128 * MIB)
    );
    assert_eq!(
        source("solid_image_cache", None),
        ("constant".into(), 32 * MIB)
    );
    assert_eq!(
        source("render_budget", None),
        ("default".into(), 1024 * MIB)
    );

    // `cache_size_source` has one series per entry, with its source.
    let line = "cache_size_source{cache=\"grid_cache\",collection=\"grib-sized\",\
                source=\"collection:[grib] grid_cache_mb\"} 1";
    assert!(metrics.contains(line), "{line}");
    let sources = metrics
        .lines()
        .filter(|l| l.starts_with("cache_size_source{"))
        .count();
    assert_eq!(sources, rows.len());
    for (kind, total) in [
        ("cache", Totals::of(&rows, Kind::Cache)),
        ("budget", Totals::of(&rows, Kind::Budget)),
    ] {
        let line = format!(
            "memory_capacity_total_bytes{{kind=\"{kind}\"}} {}",
            total.capacity_bytes
        );
        assert!(metrics.contains(&line), "{line}");
    }

    // The log block and the endpoint list the same entries and sizes.
    assert_log_and_json_agree(&rows);

    // The lightning layer's strike-window cache is built only with a
    // PostGIS collection, which needs a database; built, it is entered.
    engine_postgis::strike_window_cache_metrics();
    assert_eq!(
        plan::snapshot()
            .iter()
            .find(|r| r.entry.id == "lightning_strike_cache")
            .map(|r| r.entry.source.to_string()),
        Some("default".into())
    );
}

/// For every entry of the endpoint's document, the log block has a line
/// naming it, its collection, its capacity and its source.
fn assert_log_and_json_agree(rows: &[Row]) {
    let log = render(rows, "at boot", None);
    let json = to_json(rows, &MemoryReading::default());
    let mut lines = 0;
    for kind in ["caches", "budgets"] {
        for entry in json[kind].as_array().unwrap() {
            let id = entry["id"].as_str().unwrap();
            let name = match entry["collection"].as_str() {
                Some(collection) => format!("{id} {collection}"),
                None => id.to_string(),
            };
            let expected = format!(
                " {}  {}",
                mib(entry["capacity_bytes"].as_u64().unwrap()),
                entry["source"].as_str().unwrap()
            );
            let line = log
                .lines()
                .find(|line| {
                    line.trim_start().starts_with(&format!("{name} "))
                        && line.trim_start()[name.len()..].starts_with("  ")
                })
                .unwrap_or_else(|| panic!("no log line for {name}:\n{log}"));
            assert!(line.contains(&expected), "{line:?} disagrees with {entry}");
            lines += 1;
        }
    }
    assert_eq!(lines, rows.len());
    let caches = Totals::of(rows, Kind::Cache);
    assert_eq!(
        json["totals"]["caches"]["capacity_bytes"],
        caches.capacity_bytes
    );
    let heading = log.lines().next().unwrap();
    let expected = format!("{} caches {}", caches.count, mib(caches.capacity_bytes));
    assert!(heading.contains(&expected), "{heading}");
}

fn row(id: &'static str, scope: Scope, kind: Kind, source: SizeSource, capacity: u64) -> Row {
    let counted = |n| (kind == Kind::Cache).then_some(n);
    Row {
        entry: Entry {
            id,
            kind,
            scope,
            source,
        },
        usage: Usage {
            capacity_bytes: capacity,
            used_bytes: capacity / 2,
            entries: counted(3),
            hits: counted(5),
            misses: counted(7),
        },
    }
}

fn sample_rows() -> Vec<Row> {
    vec![
        row(
            "metatile_cache",
            Scope::Process,
            Kind::Cache,
            SizeSource::Config("[server] metatile_cache_mb"),
            8192 * MIB,
        ),
        row(
            "pvol_pixel_cache",
            Scope::Engine("odim-volume"),
            Kind::Cache,
            SizeSource::Env("MC_PVOL_PIXEL_CACHE_MB"),
            4096 * MIB,
        ),
        row(
            "tile_cache",
            Scope::Collection {
                engine: "geotiff",
                id: "opera".into(),
            },
            Kind::Cache,
            SizeSource::Default,
            256 * MIB,
        ),
        row(
            "grid_cache",
            Scope::Source {
                engine: "grib",
                id: "ifs".into(),
            },
            Kind::Cache,
            SizeSource::Collection("[grib] grid_cache_mb"),
            1536 * MIB,
        ),
        row(
            "render_budget",
            Scope::Process,
            Kind::Budget,
            SizeSource::Env("MC_RENDER_MEMORY_MB"),
            4096 * MIB,
        ),
    ]
}

/// The document's shape: an object per cache and per budget with the
/// fields the issue names, totals, and the memory gauges by name.
#[test]
fn the_endpoint_document_lists_caches_budgets_totals_and_memory() {
    let rows = sample_rows();
    let memory = MemoryReading(vec![
        ("process_resident_memory_bytes", Some(42)),
        ("jemalloc_allocated_bytes", None),
    ]);
    let json = to_json(&rows, &memory);
    assert_eq!(
        json["caches"][3],
        json!({
            "id": "grid_cache",
            "scope": "source",
            "engine": "grib",
            "collection": "ifs",
            "capacity_bytes": 1536 * MIB,
            "source": "collection:[grib] grid_cache_mb",
            "used_bytes": 768 * MIB,
            "entries": 3,
            "hits": 5,
            "misses": 7,
        })
    );
    assert_eq!(
        json["budgets"],
        json!([{
            "id": "render_budget",
            "scope": "process",
            "engine": null,
            "collection": null,
            "capacity_bytes": 4096 * MIB,
            "source": "env:MC_RENDER_MEMORY_MB",
            "used_bytes": 2048 * MIB,
            "entries": null,
            "hits": null,
            "misses": null,
        }])
    );
    let caches = (8192 + 4096 + 256 + 1536) * MIB;
    assert_eq!(
        json["totals"],
        json!({
            "caches": {"count": 4, "capacity_bytes": caches, "used_bytes": caches / 2},
            "budgets": {"count": 1, "capacity_bytes": 4096 * MIB, "used_bytes": 2048 * MIB},
        })
    );
    assert_eq!(
        json["memory"],
        json!({"process_resident_memory_bytes": 42, "jemalloc_allocated_bytes": null})
    );
    assert_log_and_json_agree(&rows);
}

/// One line per entry, grouped by scope under a heading with the totals.
#[test]
fn the_log_block_groups_entries_by_scope() {
    let log = render(&sample_rows(), "at boot", None);
    let expected = "\
Cache and memory plan at boot: 4 caches 14080 MiB (13.75 GiB), 1 budgets 4096 MiB (4.00 GiB)
  caches, process:
    metatile_cache      8192 MiB  config:[server] metatile_cache_mb
  caches, engine odim-volume:
    pvol_pixel_cache    4096 MiB  env:MC_PVOL_PIXEL_CACHE_MB
  caches, collection geotiff:
    tile_cache opera     256 MiB  default
  caches, source grib:
    grid_cache ifs      1536 MiB  collection:[grib] grid_cache_mb
  budgets, process:
    render_budget       4096 MiB  env:MC_RENDER_MEMORY_MB";
    assert_eq!(log, expected);
}

/// After a reload, a line whose size changed shows the size it had.
#[test]
fn a_changed_size_shows_the_old_one() {
    let mut rows = sample_rows();
    let previous: Logged = rows
        .iter()
        .map(|row| {
            (
                (row.entry.kind, row.entry.id, row.entry.scope.clone()),
                (row.usage.capacity_bytes, row.entry.source.clone()),
            )
        })
        .collect();
    rows[3].usage.capacity_bytes = 256 * MIB;
    rows[3].entry.source = SizeSource::Default;
    let log = render(&rows, "after reload", Some(&previous));
    let changed: Vec<&str> = log.lines().filter(|l| l.contains("(was ")).collect();
    assert_eq!(
        changed,
        ["    grid_cache ifs       256 MiB  default (was 1536 MiB)"]
    );
}

#[test]
fn sizes_print_in_mib() {
    assert_eq!(mib(0), "0 MiB");
    assert_eq!(mib(32 * MIB), "32 MiB");
    assert_eq!(mib(MIB + MIB / 2), "1.50 MiB");
}

/// The render caches' sources follow the config: the `[server]` keys, the
/// deprecated `[wms]` fallback, else the defaults.
#[test]
fn render_cache_sources_follow_the_config() {
    let mut config = ServerConfig::default_for_auto();
    assert_eq!(
        RenderCacheSources::of(&config),
        RenderCacheSources {
            metatile: SizeSource::Default,
            rendered: SizeSource::Default,
        }
    );
    config.server.metatile_cache_mb = 8192;
    config.server.rendered_cache_mb = Some(512);
    assert_eq!(
        RenderCacheSources::of(&config),
        RenderCacheSources {
            metatile: SizeSource::Config("[server] metatile_cache_mb"),
            rendered: SizeSource::Config("[server] rendered_cache_mb"),
        }
    );
    config.server.rendered_cache_mb = None;
    config.collections = vec![toml::from_str(
        "id = \"a\"\ntitle = \"t\"\ndescription = \"d\"\napis = [\"wms\"]\n\
         [wms]\nrendered_cache_mb = 512\n",
    )
    .unwrap()];
    assert_eq!(
        RenderCacheSources::of(&config).rendered,
        SizeSource::Collection("[wms] rendered_cache_mb")
    );
}

/// The endpoint takes `/admin/collections/reload`'s guard: off without a
/// token, 401 without the right one, the plan with it. A collection the
/// accepted config no longer has is left out, though its engine lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_endpoint_needs_the_admin_token_and_lists_live_collections() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server]\nhost = \"127.0.0.1\"\nport = 8000\n\
             [[collections]]\nid = \"radar\"\ntitle = \"t\"\ndescription = \"d\"\n\
             engine_type = \"geotiff\"\ndata_path = {:?}\napis = [\"wms\"]\n\
             [collections.geotiff]\nfilename_template = \"radar_%Y%m%dT%H%MZ.tif\"\n\
             parameter = \"reflectivity\"\nunit = \"dBZ\"\n",
            testdata("radar").to_str().unwrap()
        ),
    )
    .unwrap();
    let mut state = crate::watcher::tests::build_state(&config_path);
    let gone = Arc::new(());
    plan::register(
        Entry::cache(
            "tile_cache",
            Scope::Collection {
                engine: "geotiff",
                id: "gone".into(),
            },
            SizeSource::Default,
        ),
        &gone,
        |_| Usage::default(),
    );
    let call = |state: &AdminState, token: Option<&str>| {
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            headers.insert(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
        }
        handler(State(state.clone()), headers)
    };
    let status = |result: Result<Json<Value>, (StatusCode, Json<Value>)>| match result {
        Ok(_) => StatusCode::OK,
        Err((status, _)) => status,
    };
    assert_eq!(status(call(&state, Some("t")).await), StatusCode::FORBIDDEN);
    Arc::get_mut(&mut state).unwrap().admin_token = Some("secret".into());
    assert_eq!(status(call(&state, None).await), StatusCode::UNAUTHORIZED);
    assert_eq!(
        status(call(&state, Some("wrong")).await),
        StatusCode::UNAUTHORIZED
    );
    let Json(plan) = call(&state, Some("secret")).await.unwrap();
    let tile_caches: Vec<&str> = plan["caches"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["id"] == "tile_cache")
        .map(|c| c["collection"].as_str().unwrap())
        .collect();
    assert_eq!(tile_caches, ["radar"]);
    assert!(plan["budgets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["id"] == "render_budget"));
    assert!(plan["memory"]
        .get("process_resident_memory_bytes")
        .is_some());
    drop(gone);
}
