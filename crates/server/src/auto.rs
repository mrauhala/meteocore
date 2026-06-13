//! Auto-collection discovery (#411, phase 1).
//!
//! Turns a directory tree of data files into synthesized [`CollectionConfig`]s
//! for the `--auto-collections <DIR>` flag — no `config.toml` required. The
//! mapping is **per-subdirectory + loose files**: each immediate subdirectory
//! of a root becomes a collection (or, for the single-file formats, one per
//! file), and data files sitting loose in the root are grouped the same way
//! under the root's name.
//!
//! Phase 1 covers the **self-describing / directory-native** engines (time and
//! parameters come from inside the data or a directory listing, so no filename
//! templating is needed): `zarr`, `grib` (with index sidecars), `querydata`,
//! and the single-file `csv` / `geojson`. `geotiff` and `odim` encode time in
//! the filename and need template inference + (for ODIM) a COMP/PVOL probe —
//! deferred to phase 2; they are detected and skipped with a log here.
//!
//! The synthesized configs are appended to the live config and run through the
//! same [`ServerConfig::validate`](ds_core::config::ServerConfig::validate) as
//! TOML collections (so e.g. duplicate ids are rejected uniformly).

use ds_core::config::{CollectionConfig, GribConfig, QueryDataConfig, ZarrConfig};
use std::path::Path;

/// Scan each root directory and return the synthesized collection configs.
///
/// Unrecognized directories, GRIB without index sidecars, and the phase-2
/// formats are skipped with a `warn!`/`info!` log rather than failing — one bad
/// directory should not sink the whole launch.
pub fn scan_roots(roots: &[String]) -> Vec<CollectionConfig> {
    let mut out = Vec::new();
    for root in roots {
        match scan_one_root(Path::new(root)) {
            Ok(mut cfgs) => {
                tracing::info!(
                    "--auto-collections: '{root}' yielded {} collection(s)",
                    cfgs.len()
                );
                out.append(&mut cfgs);
            }
            Err(e) => tracing::warn!("--auto-collections: skipping '{root}': {e}"),
        }
    }
    out
}

/// Scan one root: classify each immediate subdirectory, then the loose files.
fn scan_one_root(root: &Path) -> Result<Vec<CollectionConfig>, String> {
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()));
    }
    let mut subdirs = Vec::new();
    let mut loose_files = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.is_file() {
            loose_files.push(path);
        }
    }
    subdirs.sort();
    loose_files.sort();

    let mut out = Vec::new();
    // Each immediate subdirectory => its own collection(s).
    for subdir in &subdirs {
        let hint = file_name(subdir);
        out.extend(classify_dir(subdir, &hint));
    }
    // Loose data files in the root => collection(s) named after the root.
    let root_hint = file_name(root);
    out.extend(classify_files(&loose_files, root, &root_hint));
    Ok(out)
}

/// Classify a directory (a subdirectory of a root) into zero or more
/// collections. A Zarr store is recognized by the directory itself; everything
/// else is decided from the files directly inside it.
fn classify_dir(dir: &Path, id_hint: &str) -> Vec<CollectionConfig> {
    if dir_is_zarr_store(dir) {
        return match mk_zarr(dir) {
            Some(c) => vec![c],
            None => vec![],
        };
    }
    let files: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .collect(),
        Err(e) => {
            tracing::warn!("--auto-collections: cannot read {}: {e}", dir.display());
            return vec![];
        }
    };
    classify_files(&files, dir, id_hint)
}

/// Classify a set of files in `dir` (first match wins, per the detection order
/// in #411). `dir` is the data directory passed to directory-native engines;
/// `id_hint` names a directory-native collection (single-file formats are named
/// per file instead).
fn classify_files(
    files: &[std::path::PathBuf],
    dir: &Path,
    id_hint: &str,
) -> Vec<CollectionConfig> {
    if files.is_empty() {
        return vec![];
    }

    // 1. QueryData — a directory of .sqd (time + params read from the file).
    if files.iter().any(|f| has_ext(f, "sqd")) {
        return mk_querydata(dir, id_hint).into_iter().collect();
    }

    // 2. GRIB — .grib2/.grb2 *with* index sidecars (the engine never builds them).
    let data_suffix = if files.iter().any(|f| has_ext(f, "grib2")) {
        Some(".grib2")
    } else if files.iter().any(|f| has_ext(f, "grb2")) {
        Some(".grb2")
    } else {
        None
    };
    if let Some(data_suffix) = data_suffix {
        // `.index` => ECMWF JSON-lines (engine default); `.idx` => wgrib2.
        let index = if files.iter().any(|f| has_ext(f, "index")) {
            Some((".index", Some("ecmwf-json")))
        } else if files.iter().any(|f| has_ext(f, "idx")) {
            Some((".idx", Some("wgrib2")))
        } else {
            None
        };
        return match index {
            Some((index_suffix, fmt)) => mk_grib(dir, id_hint, index_suffix, data_suffix, fmt)
                .into_iter()
                .collect(),
            None => {
                tracing::warn!(
                    "--auto-collections: {} has GRIB2 data but no .index/.idx sidecars — \
                     GRIB needs prebuilt indexes; skipping",
                    dir.display()
                );
                vec![]
            }
        };
    }

    // 3. GeoTIFF — phase 2 (filename-template inference).
    if files
        .iter()
        .any(|f| has_ext(f, "tif") || has_ext(f, "tiff"))
    {
        tracing::info!(
            "--auto-collections: {} looks like GeoTIFF — auto-detection is phase 2 (#411); skipping",
            dir.display()
        );
        return vec![];
    }

    // 4. ODIM HDF5 — phase 2 (COMP/PVOL probe + template inference).
    if files.iter().any(|f| has_ext(f, "h5") || has_ext(f, "hdf5")) {
        tracing::info!(
            "--auto-collections: {} looks like ODIM HDF5 — auto-detection is phase 2 (#411); skipping",
            dir.display()
        );
        return vec![];
    }

    // 5 + 6. Single-file leaf formats — one collection per file. GeoJSON and
    // CSV don't conflict (unlike the directory-native formats above, which are
    // first-match-wins), so a directory holding both yields both.
    let mut single = Vec::new();
    single.extend(
        files
            .iter()
            .filter(|f| has_ext(f, "geojson"))
            .filter_map(|f| mk_geojson(f)),
    );
    single.extend(
        files
            .iter()
            .filter(|f| has_ext(f, "csv"))
            .filter_map(|f| mk_csv(f)),
    );
    if !single.is_empty() {
        return single;
    }

    tracing::info!(
        "--auto-collections: no recognized data files in {}; skipping",
        dir.display()
    );
    vec![]
}

// ---- engine-specific synthesizers -----------------------------------------

fn mk_zarr(dir: &Path) -> Option<CollectionConfig> {
    let path = path_str(dir)?;
    // Strip a trailing ".zarr" from the store directory name for the id.
    let raw = file_name(dir);
    let stem = raw.strip_suffix(".zarr").unwrap_or(&raw);
    let id = slugify(stem);
    Some(mk_collection(
        id,
        "zarr",
        vec!["edr".into()],
        None,
        Some(ZarrConfig::auto_local(path)),
        None,
        None,
        &dir.display().to_string(),
    ))
}

fn mk_querydata(dir: &Path, id_hint: &str) -> Option<CollectionConfig> {
    let path = path_str(dir)?;
    Some(mk_collection(
        slugify(id_hint),
        "querydata",
        vec!["edr".into()],
        Some(path), // querydata reads collection.data_path
        None,
        None,
        Some(QueryDataConfig::auto_default()),
        &dir.display().to_string(),
    ))
}

fn mk_grib(
    dir: &Path,
    id_hint: &str,
    index_suffix: &str,
    data_suffix: &str,
    index_format: Option<&str>,
) -> Option<CollectionConfig> {
    let path = path_str(dir)?;
    Some(mk_collection(
        slugify(id_hint),
        "grib",
        vec!["edr".into()],
        None, // grib reads GribConfig.data_path
        None,
        Some(GribConfig::auto_local(
            path,
            index_suffix.to_string(),
            data_suffix.to_string(),
            index_format.map(|s| s.to_string()),
        )),
        None,
        &dir.display().to_string(),
    ))
}

fn mk_geojson(file: &Path) -> Option<CollectionConfig> {
    let path = path_str(file)?;
    let id = slugify(&file_stem(file));
    Some(mk_collection(
        id,
        "geojson",
        vec!["features".into()],
        Some(path),
        None,
        None,
        None,
        &file.display().to_string(),
    ))
}

fn mk_csv(file: &Path) -> Option<CollectionConfig> {
    let path = path_str(file)?;
    let id = slugify(&file_stem(file));
    Some(mk_collection(
        id,
        "csv",
        vec!["edr".into(), "features".into()],
        Some(path),
        None,
        None,
        None,
        &file.display().to_string(),
    ))
}

/// Build a `CollectionConfig` with the auto-discovery defaults (no keywords /
/// license / preview, the relevant engine sub-table set, the rest `None`). The
/// title is humanized from the id; the description records the source.
#[allow(clippy::too_many_arguments)]
fn mk_collection(
    id: String,
    engine_type: &str,
    apis: Vec<String>,
    data_path: Option<String>,
    zarr: Option<ZarrConfig>,
    grib: Option<GribConfig>,
    querydata: Option<QueryDataConfig>,
    source: &str,
) -> CollectionConfig {
    CollectionConfig {
        title: humanize(&id),
        description: format!("Auto-generated collection from {source}"),
        id,
        keywords: Vec::new(),
        license: None,
        data_path,
        apis,
        engine_type: engine_type.to_string(),
        geotiff: None,
        querydata,
        grib,
        zarr,
        odim: None,
        wms: None,
        postgis: None,
        preview: None,
    }
}

// ---- helpers ---------------------------------------------------------------

/// A directory is a Zarr store if its name ends in `.zarr` or it holds Zarr
/// group/array metadata (`zarr.json` for V3, `.zgroup`/`.zarray` for V2).
fn dir_is_zarr_store(dir: &Path) -> bool {
    file_name(dir).to_ascii_lowercase().ends_with(".zarr")
        || dir.join("zarr.json").exists()
        || dir.join(".zgroup").exists()
        || dir.join(".zarray").exists()
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

fn path_str(path: &Path) -> Option<String> {
    match path.to_str() {
        Some(s) => Some(s.to_string()),
        None => {
            tracing::warn!(
                "--auto-collections: skipping non-UTF-8 path {}",
                path.display()
            );
            None
        }
    }
}

/// Lowercase, map every run of non-`[a-z0-9]` to a single `-`, trim leading and
/// trailing `-`. Keeps ids URL-safe (aligns with #305).
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_dash = false;
    for c in s.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(lc);
        } else {
            pending_dash = true;
        }
    }
    out
}

/// Title-case a slug for the collection title (e.g. `radar-fmi` -> `Radar Fmi`).
/// Falls back to the slug if it has no word characters.
fn humanize(slug: &str) -> String {
    let title = slug
        .split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        slug.to_string()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"").unwrap();
    }

    fn ids(cfgs: &[CollectionConfig]) -> Vec<(String, String)> {
        cfgs.iter()
            .map(|c| (c.id.clone(), c.engine_type.clone()))
            .collect()
    }

    #[test]
    fn slugify_and_humanize() {
        assert_eq!(slugify("Radar_FMI 2"), "radar-fmi-2");
        assert_eq!(slugify("era5.zarr"), "era5-zarr");
        assert_eq!(slugify("--weird__name--"), "weird-name");
        assert_eq!(humanize("radar-fmi"), "Radar Fmi");
    }

    #[test]
    fn classifies_subdirs_per_format() {
        let root = TempDir::new().unwrap();
        let r = root.path();

        // querydata subdir
        let qd = r.join("meps");
        fs::create_dir(&qd).unwrap();
        touch(&qd, "20260405T180000Z_meps.sqd");

        // grib subdir with index sidecars
        let grib = r.join("ecmwf");
        fs::create_dir(&grib).unwrap();
        touch(&grib, "run.grib2");
        touch(&grib, "run.index");

        // grib subdir WITHOUT index -> skipped
        let bad = r.join("noidx");
        fs::create_dir(&bad).unwrap();
        touch(&bad, "x.grib2");

        // zarr store (V3 marker)
        let zarr = r.join("era5.zarr");
        fs::create_dir(&zarr).unwrap();
        touch(&zarr, "zarr.json");

        let cfgs = scan_roots(&[r.to_str().unwrap().to_string()]);
        let got = ids(&cfgs);
        assert!(
            got.contains(&("meps".into(), "querydata".into())),
            "{got:?}"
        );
        assert!(got.contains(&("ecmwf".into(), "grib".into())), "{got:?}");
        assert!(got.contains(&("era5".into(), "zarr".into())), "{got:?}");
        // grib-without-index produced nothing
        assert!(!got.iter().any(|(id, _)| id == "noidx"), "{got:?}");
    }

    #[test]
    fn grib_index_format_inferred_from_suffix() {
        let root = TempDir::new().unwrap();
        let gfs = root.path().join("gfs");
        fs::create_dir(&gfs).unwrap();
        touch(&gfs, "f006.grib2");
        touch(&gfs, "f006.idx");

        let cfgs = scan_roots(&[root.path().to_str().unwrap().to_string()]);
        let grib = cfgs.iter().find(|c| c.engine_type == "grib").unwrap();
        let g = grib.grib.as_ref().unwrap();
        assert_eq!(g.index_format.as_deref(), Some("wgrib2"));
        assert_eq!(g.index_suffix.as_deref(), Some(".idx"));
        assert_eq!(g.data_suffix.as_deref(), Some(".grib2"));
        assert!(g.data_path.is_some());
    }

    #[test]
    fn loose_single_files_are_one_collection_each() {
        let root = TempDir::new().unwrap();
        let r = root.path();
        touch(r, "weather.csv");
        touch(r, "stations.csv");
        touch(r, "regions.geojson");

        let cfgs = scan_roots(&[r.to_str().unwrap().to_string()]);
        let got = ids(&cfgs);
        // GeoJSON and CSV are independent single-file formats: a directory with
        // both yields one collection per file of each.
        assert!(
            got.contains(&("regions".into(), "geojson".into())),
            "{got:?}"
        );
        assert!(got.contains(&("weather".into(), "csv".into())), "{got:?}");
        assert!(got.contains(&("stations".into(), "csv".into())), "{got:?}");
        assert_eq!(got.len(), 3, "{got:?}");
    }

    #[test]
    fn loose_csv_only_emits_per_file() {
        let root = TempDir::new().unwrap();
        let r = root.path();
        touch(r, "weather.csv");
        touch(r, "stations.csv");

        let cfgs = scan_roots(&[r.to_str().unwrap().to_string()]);
        let mut got = ids(&cfgs);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("stations".to_string(), "csv".to_string()),
                ("weather".to_string(), "csv".to_string()),
            ]
        );
        // csv enables both EDR and Features
        let csv = cfgs.iter().find(|c| c.id == "weather").unwrap();
        assert_eq!(csv.apis, vec!["edr".to_string(), "features".to_string()]);
        assert!(csv.data_path.as_deref().unwrap().ends_with("weather.csv"));
    }

    #[test]
    fn empty_and_unknown_dirs_yield_nothing() {
        let root = TempDir::new().unwrap();
        let r = root.path();
        let empty = r.join("empty");
        fs::create_dir(&empty).unwrap();
        let junk = r.join("junk");
        fs::create_dir(&junk).unwrap();
        touch(&junk, "readme.txt");

        let cfgs = scan_roots(&[r.to_str().unwrap().to_string()]);
        assert!(cfgs.is_empty(), "{:?}", ids(&cfgs));
    }

    #[test]
    fn geotiff_and_odim_are_deferred_to_phase2() {
        let root = TempDir::new().unwrap();
        let r = root.path();
        let tif = r.join("radar-tif");
        fs::create_dir(&tif).unwrap();
        touch(&tif, "radar_20260101T0000Z.tif");
        let h5 = r.join("radar-h5");
        fs::create_dir(&h5).unwrap();
        touch(&h5, "202601011200_radar.h5");

        let cfgs = scan_roots(&[r.to_str().unwrap().to_string()]);
        assert!(
            cfgs.is_empty(),
            "phase-2 formats must be skipped: {:?}",
            ids(&cfgs)
        );
    }
}
