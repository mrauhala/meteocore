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
//! The rules read only a basename. A store-internal listing is not a data
//! scan: Zarr v2 keeps its metadata in `.zarray` / `.zattrs` / `.zgroup`.

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
/// path.
pub fn is_temporary_key(key: &str) -> bool {
    is_temporary(key.rsplit('/').next().unwrap_or(key))
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
        // A hidden directory does not hide the files under it: the rules
        // read the basename only.
        assert!(!is_temporary_key(".cache/202610090000_fivih_PVOL.h5"));
    }
}
