//! File names a data scan never reads (#1009): the temporary names a
//! publisher writes under before renaming a finished file into place.
//!
//! rsync and most atomic writers write a hidden `.name` and rename it once
//! complete; others append `.tmp` or `.part`. A poll that lists the
//! directory in between reads a truncated file: a spurious ERROR and, for a
//! 1 GB model run, a wasted read, while the finished file arrives under its
//! own name on the next poll anyway. Every engine that enumerates a
//! directory or object prefix itself skips these names with
//! [`is_temporary`]; the shared catalog scan in `ds_storage::discovery`
//! skips [`is_hidden`] names and leaves the suffixes to a collection's
//! `exclude_patterns`, whose defaults are [`PARTIAL_SUFFIXES`].
//!
//! A listing that walks folders below its base (an object-store prefix, or
//! a local directory through `ds_storage`, whose `list` is recursive) also
//! skips every file inside a hidden folder below that base (#1029):
//! [`in_hidden_folder`], or [`is_temporary_below`] for both rules at once.
//! rsync `--delay-updates` stages a whole transfer in `.~tmp~/` next to its
//! destination and renames the files into place at the end, under their
//! final basenames. The base itself is never judged: data under
//! `~/.local/share/…` or a bucket prefix `.archive/` is read.
//!
//! A store-internal listing is not a data scan: Zarr v2 keeps its metadata
//! in `.zarray` / `.zattrs` / `.zgroup`.

/// Suffixes of a file that is still being written. A collection's default
/// `exclude_patterns` are these as `*.tmp` / `*.part` globs.
pub const PARTIAL_SUFFIXES: [&str; 2] = [".tmp", ".part"];

/// [`PARTIAL_SUFFIXES`] as exclude globs, `["*.tmp", "*.part"]`: a
/// collection's default `exclude_patterns`, and the exclusions of a
/// catalog scan whose engine has no such setting.
pub fn partial_exclude_patterns() -> Vec<String> {
    PARTIAL_SUFFIXES
        .iter()
        .map(|suffix| format!("*{suffix}"))
        .collect()
}

/// Whether `basename` is hidden, i.e. starts with `.`, the temporary name of
/// a file written and then renamed into place.
pub fn is_hidden(basename: &str) -> bool {
    basename.starts_with('.')
}

/// Whether `basename` is a temporary name: hidden ([`is_hidden`]) or ending
/// in one of the [`PARTIAL_SUFFIXES`]. Case-sensitive, like a collection's
/// `exclude_patterns`.
pub fn is_temporary(basename: &str) -> bool {
    is_hidden(basename) || PARTIAL_SUFFIXES.iter().any(|s| basename.ends_with(s))
}

/// [`is_temporary`] for the last segment of a `/`-separated object key or
/// path. It reads the basename only: a listing that descends below its base
/// uses [`is_temporary_below`].
pub fn is_temporary_key(key: &str) -> bool {
    is_temporary(key.rsplit('/').next().unwrap_or(key))
}

/// Whether `key`, an object key or `/`-separated path listed under `base`,
/// lies inside a hidden folder below `base`: a segment after `base`'s own
/// segments, other than the basename, is [`is_hidden`] (#1029).
///
/// `base` is the listed prefix or directory, compared segment by segment,
/// so a trailing `/` does not matter. Its own segments are never judged:
/// they are the configured location, which may legitimately be a dot folder.
/// A `key` not under `base` is `false`, left to its basename's rules.
///
/// Only the hidden rule applies to folders. [`PARTIAL_SUFFIXES`] name a
/// file that is still being written; a folder ending in `.tmp` is not
/// in progress, and the shared catalog scan leaves those suffixes to a
/// collection's `exclude_patterns`, which match basenames.
pub fn in_hidden_folder(base: &str, key: &str) -> bool {
    let mut segments = key.split('/').filter(|s| !s.is_empty());
    for base_segment in base.split('/').filter(|s| !s.is_empty()) {
        if segments.next() != Some(base_segment) {
            return false;
        }
    }
    let mut below = segments.peekable();
    while let Some(segment) = below.next() {
        if below.peek().is_some() && is_hidden(segment) {
            return true;
        }
    }
    false
}

/// Whether a listing of `base` skips the listed `key`: a temporary basename
/// ([`is_temporary_key`]) or a hidden folder below `base`
/// ([`in_hidden_folder`]). The rule for an engine that enumerates a prefix
/// or directory with folders below it itself.
pub fn is_temporary_below(base: &str, key: &str) -> bool {
    is_temporary_key(key) || in_hidden_folder(base, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_names_are_temporary() {
        assert!(is_hidden(".2026-10-09T00:00:00Z_ecmwf.sqd"));
        assert!(is_temporary(".2026-10-09T00:00:00Z_ecmwf.sqd"));
        assert!(is_temporary(".202605150000_fivih_PVOL.h5.Xa81Bc"));
        assert!(!is_hidden("2026-10-09T00:00:00Z_ecmwf.sqd"));
    }

    #[test]
    fn partial_suffixes_are_temporary_only_at_the_end() {
        assert!(is_temporary("run.sqd.tmp"));
        assert!(is_temporary("radar.tif.part"));
        assert!(!is_temporary("radar.part.tif"));
        assert!(!is_temporary("202610090000_ecmwf.sqd"));
        // Case-sensitive, like `exclude_patterns`.
        assert!(!is_temporary("run.sqd.TMP"));
    }

    #[test]
    fn partial_exclude_patterns_glob_the_suffixes() {
        assert_eq!(partial_exclude_patterns(), ["*.tmp", "*.part"]);
    }

    #[test]
    fn keys_are_judged_by_their_last_segment() {
        assert!(is_temporary_key("2026/10/09/.202610090000_fivih_PVOL.h5"));
        assert!(is_temporary_key(".hidden"));
        assert!(!is_temporary_key("2026/10/09/202610090000_fivih_PVOL.h5"));
        // The basename only: folders are `in_hidden_folder`'s.
        assert!(!is_temporary_key(".cache/202610090000_fivih_PVOL.h5"));
    }

    /// rsync `--delay-updates` stages files in `.~tmp~/` under their final
    /// names: a hidden folder below the base hides them, at any depth.
    #[test]
    fn hidden_folders_below_the_base_hide_their_files() {
        let name = "202610090000_fivih_PVOL.h5";
        assert!(in_hidden_folder("", &format!(".~tmp~/{name}")));
        assert!(in_hidden_folder(
            "2026/10/09",
            &format!("2026/10/09/.~tmp~/{name}")
        ));
        assert!(in_hidden_folder("d", &format!("d/fivih/.staging/{name}")));
        // Nested: a hidden folder anywhere between the base and the file.
        assert!(in_hidden_folder("d", &format!("d/.cache/2026/10/{name}")));
        assert!(in_hidden_folder("d", &format!("d/2026/.cache/10/{name}")));
        // A trailing `/` on the base changes nothing.
        assert!(in_hidden_folder("d/", &format!("d/.~tmp~/{name}")));
    }

    #[test]
    fn normal_subfolders_and_the_basename_are_not_hidden_folders() {
        let name = "202610090000_fivih_PVOL.h5";
        assert!(!in_hidden_folder("", name));
        assert!(!in_hidden_folder("d", &format!("d/{name}")));
        assert!(!in_hidden_folder("d", &format!("d/2026/10/09/{name}")));
        // A hidden basename is the file rule's, not a folder.
        assert!(!in_hidden_folder("d", &format!("d/2026/.{name}")));
        // Partial suffixes are file rules only.
        assert!(!in_hidden_folder("d", &format!("d/2026.tmp/{name}")));
        assert!(!in_hidden_folder("d", &format!("d/x.part/{name}")));
    }

    /// The configured base may itself be, or sit in, a dot folder.
    #[test]
    fn dot_folders_in_the_base_are_kept() {
        let name = "radar_20260324T2315Z.tif";
        assert!(!in_hidden_folder(".archive", &format!(".archive/{name}")));
        assert!(!in_hidden_folder(
            "home/u/.local/share/x",
            &format!("home/u/.local/share/x/2026/{name}")
        ));
        assert!(!in_hidden_folder(".a/.b/", &format!(".a/.b/{name}")));
        // Below such a base, a hidden folder still hides.
        assert!(in_hidden_folder(
            ".archive",
            &format!(".archive/.~tmp~/{name}")
        ));
        // A key not under the base is left to its basename's rules.
        assert!(!in_hidden_folder(
            "other",
            &format!(".archive/.~tmp~/{name}")
        ));
        // Segments compare whole: `.arch` is not the base `.archive`.
        assert!(!in_hidden_folder(".arch", &format!(".archive/{name}")));
    }

    #[test]
    fn temporary_below_combines_the_basename_and_folder_rules() {
        let name = "202610090000_fivih_PVOL.h5";
        assert!(!is_temporary_below("d", &format!("d/2026/{name}")));
        assert!(is_temporary_below("d", &format!("d/.~tmp~/{name}")));
        assert!(is_temporary_below("d", &format!("d/2026/.{name}")));
        assert!(is_temporary_below("d", &format!("d/2026/{name}.part")));
        assert!(!is_temporary_below(".archive", &format!(".archive/{name}")));
    }
}
