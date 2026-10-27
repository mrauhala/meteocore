//! The poll's scan under a cap of one open per poll: a file replaced in
//! place keeps drawing its old version until its new version is open, and
//! the content version moves with the catalog that serves the new one.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ds_core::map_engine::{MapEngine, OutputCrs};

use super::*;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/meps-netcdf")
        .join(name)
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// Put `source`'s fields under `name` in `dir`, its valid time `from`
/// moved to `to`, by renaming a finished file over the old one, as a
/// publisher does: the old file's open handle still reads the old bytes.
fn replace(dir: &Path, name: &str, source: &str, from: &str, to: &str) {
    let mut bytes = std::fs::read(fixture(source)).unwrap();
    let (from, to) = (
        (at(from).timestamp() as f64).to_le_bytes(),
        (at(to).timestamp() as f64).to_le_bytes(),
    );
    let offsets: Vec<usize> = (0..bytes.len() - 8)
        .filter(|&i| bytes[i..i + 8] == from)
        .collect();
    assert_eq!(offsets.len(), 1, "the valid time is stored once");
    bytes[offsets[0]..offsets[0] + 8].copy_from_slice(&to);
    let temporary = dir.join(format!(".{name}"));
    std::fs::write(&temporary, &bytes).unwrap();
    std::fs::rename(&temporary, dir.join(name)).unwrap();
}

#[test]
fn a_replaced_file_draws_its_old_version_until_its_new_one_opens() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["meps_sfc_00_20261015T06Z.nc", "meps_sfc_01_20261015T06Z.nc"] {
        std::fs::copy(fixture(name), dir.path().join(name)).unwrap();
    }
    let config: NetcdfConfig = toml::from_str(&format!(
        r#"data_path = {:?}
           filename_template = "meps_sfc_{{lead}}_%Y%m%dT{{run}}Z.nc"
           variables = ["air_temperature_2m"]"#,
        dir.path().display().to_string()
    ))
    .unwrap();
    let engine = NetcdfEngine::new("meps", &config).unwrap();
    let draw = |time: &str| {
        let extent = engine.raster_info().spatial_extent.unwrap();
        let tile = engine
            .get_raster_tile(
                extent,
                8,
                8,
                Some(at(time)),
                &OutputCrs::Wgs84,
                Some("air_temperature_2m"),
                None,
                None,
            )
            .unwrap();
        (0..64).map(|i| tile.values.value_at(i)).collect::<Vec<_>>()
    };
    let (six, seven) = ("2026-10-15T06:00:00Z", "2026-10-15T07:00:00Z");
    let old = (draw(six), draw(seven));
    assert_ne!(old.0, old.1);
    let version = engine.content_version();

    // Both leads replaced: 00 by lead 02's fields at 06Z, 01 by lead 03's
    // at 07Z.
    let lead_00 = "meps_sfc_00_20261015T06Z.nc";
    let lead_01 = "meps_sfc_01_20261015T06Z.nc";
    replace(
        dir.path(),
        lead_00,
        "meps_sfc_02_20261015T06Z.nc",
        "2026-10-15T08:00:00Z",
        six,
    );
    replace(
        dir.path(),
        lead_01,
        "meps_sfc_03_20261015T06Z.nc",
        "2026-10-15T09:00:00Z",
        seven,
    );
    let one = Limits {
        opens: 1,
        concurrency: 1,
        until: None,
    };

    // The newest name first: lead 00 is opened, lead 01 waits and its old
    // version is still drawn, at no cost to the catalog.
    engine.scan(&one, 1).unwrap();
    assert!(engine.content_version() > version);
    assert_ne!(draw(six), old.0, "lead 00's new version");
    assert_eq!(draw(seven), old.1, "lead 01's old version, still served");
    assert_eq!(engine.times(), [at(six), at(seven)]);

    let version = engine.content_version();
    engine.scan(&one, 1).unwrap();
    assert!(engine.content_version() > version);
    assert_ne!(draw(seven), old.1, "lead 01's new version");

    let version = engine.content_version();
    engine.scan(&one, 1).unwrap();
    assert_eq!(engine.content_version(), version, "nothing changed");
}

/// A run whose files are all closed never displaces a run that serves
/// (#1290). With `max_runs = 1`, a newer run closed (the runs newer than
/// it were withdrawn, say, and opening it again failed) and an older one
/// open are both kept: the open one serves, the closed one is opened
/// again. Once a file of the newer run is open, it alone is kept.
#[test]
fn a_closed_run_never_displaces_one_that_serves() {
    let dir = tempfile::tempdir().unwrap();
    let config: NetcdfConfig = toml::from_str(&format!(
        "data_path = {:?}\nmax_runs = 1",
        dir.path().display().to_string()
    ))
    .unwrap();
    let engine = NetcdfEngine::new("meps", &config).unwrap();
    let (six, nine) = (at("2026-10-15T06:00:00Z"), at("2026-10-15T09:00:00Z"));
    let open = |name: &str, run: DateTime<Utc>| {
        let path = fixture(name);
        Known::Open(Arc::new(OpenFile {
            id: next_file_id(),
            path: path.clone(),
            identity: Identity::sized(1),
            reader: ingest::open(&path).unwrap(),
            run: Some(run),
            times: vec![run],
            reads: Vec::new(),
            heap_bytes: 0,
        }))
    };
    let older = fixture("meps_sfc_00_20261015T06Z.nc");
    let newer = fixture("meps_sfc_00_20261015T09Z.nc");
    let mut state = PollState {
        known: HashMap::from([
            (older.clone(), open("meps_sfc_00_20261015T06Z.nc", six)),
            (
                newer.clone(),
                Known::Closed {
                    identity: Identity::sized(1),
                    run: Some(nine),
                    newest: nine,
                },
            ),
        ]),
        layout: None,
        content_version: 0,
        warned_mixed: false,
        prewarmed: HashSet::new(),
    };
    assert_eq!(
        engine.retained(&mut state),
        HashSet::from([older.clone(), newer.clone()])
    );

    state
        .known
        .insert(newer.clone(), open("meps_sfc_00_20261015T09Z.nc", nine));
    assert_eq!(engine.retained(&mut state), HashSet::from([newer]));
}
