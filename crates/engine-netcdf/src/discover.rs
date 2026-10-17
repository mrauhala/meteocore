//! Listing the data directory.
//!
//! With a `filename_template` the listing is ds-storage's `scan_local` over
//! its `FilenameMatcher`, which reads `{run}` as the timestamp's hour and
//! `{lead}` as a lead and keeps one file per (timestamp, lead): every lead
//! of a MEPS run. Without one, every `*.nc` name. Either way a file's valid
//! times and run come from its own variables; the name only selects the
//! file and orders the opening.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use ds_storage::discovery::{scan_local, FilenameMatcher, ScanSpec, Symlinks};

use crate::catalog::Identity;

/// A file the listing found.
#[derive(Debug, Clone)]
pub(crate) struct Listed {
    pub path: PathBuf,
    pub identity: Identity,
}

/// Which names in the directory are the collection's files.
pub(crate) enum Names {
    /// Those matching a `filename_template`.
    Template(FilenameMatcher),
    /// Every name ending in `.nc`.
    NetcdfSuffix,
}

/// The regular files of `dir` (non-recursive; a symlink to a file counts)
/// that `names` selects, skipping the names a publisher writes before
/// renaming a finished file into place (`ds_core::temp_files`). The newest
/// first, so a poll that cannot open everything opens the newest files:
/// by the time and lead the name encodes for a template, else by
/// modification time.
pub(crate) fn list(dir: &Path, names: &Names, label: &str) -> std::io::Result<Vec<Listed>> {
    let identity = |metadata: &std::fs::Metadata| Identity {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    };
    match names {
        Names::Template(matcher) => {
            let exclude = ds_core::temp_files::partial_exclude_patterns();
            let spec = ScanSpec {
                exclude: &exclude,
                symlinks: Symlinks::Follow,
                ..ScanSpec::new(matcher, label)
            };
            let mut files = scan_local(dir, &spec)?;
            files.sort_by_key(|file| (Reverse(file.time), file.lead));
            Ok(files
                .into_iter()
                .map(|file| Listed {
                    identity: identity(&file.metadata),
                    path: file.path,
                })
                .collect())
        }
        Names::NetcdfSuffix => {
            let mut found = Vec::new();
            for entry in std::fs::read_dir(dir)? {
                let Ok(entry) = entry else {
                    continue;
                };
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if !name.ends_with(".nc") || ds_core::temp_files::is_temporary(&name) {
                    continue;
                }
                let path = entry.path();
                let Ok(metadata) = std::fs::metadata(&path) else {
                    continue;
                };
                if metadata.is_file() {
                    found.push(Listed {
                        identity: identity(&metadata),
                        path,
                    });
                }
            }
            found.sort_by(|a, b| {
                b.identity
                    .modified
                    .cmp(&a.identity.modified)
                    .then_with(|| b.path.cmp(&a.path))
            });
            Ok(found)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(listed: Vec<Listed>) -> Vec<String> {
        listed
            .into_iter()
            .map(|l| l.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn lists_finished_netcdf_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "a.nc",
            "b.nc",
            ".c.nc",
            "d.nc.tmp",
            "e.nc.part",
            "f.grib2",
            "g.nc4",
        ] {
            std::fs::write(dir.path().join(name), name).unwrap();
        }
        std::fs::create_dir(dir.path().join("sub.nc")).unwrap();
        let mut all = names(list(dir.path(), &Names::NetcdfSuffix, "t").unwrap());
        all.sort();
        assert_eq!(all, ["a.nc", "b.nc"]);
        assert!(list(&dir.path().join("missing"), &Names::NetcdfSuffix, "t").is_err());
    }

    /// A per-lead family: every lead of every run, the newest run first,
    /// whatever else lies in the directory.
    #[test]
    fn a_template_keeps_every_lead_newest_run_first() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "meps_sfc_00_20261015T06Z.nc",
            "meps_sfc_01_20261015T06Z.nc",
            "meps_sfc_00_20261015T09Z.nc",
            "meps_sfc_01_20261015T09Z.nc",
            "meps_pl_00_20261015T09Z.nc",
            ".meps_sfc_02_20261015T09Z.nc",
            "meps_sfc_02_20261015T09Z.nc.part",
        ] {
            std::fs::write(dir.path().join(name), name).unwrap();
        }
        let matcher = FilenameMatcher::from_template("meps_sfc_{lead}_%Y%m%dT{run}Z.nc").unwrap();
        let listed = list(dir.path(), &Names::Template(matcher), "t").unwrap();
        assert_eq!(
            names(listed),
            [
                "meps_sfc_00_20261015T09Z.nc",
                "meps_sfc_01_20261015T09Z.nc",
                "meps_sfc_00_20261015T06Z.nc",
                "meps_sfc_01_20261015T06Z.nc",
            ]
        );
    }
}
