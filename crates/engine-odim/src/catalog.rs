//! Filename-template parsing and local-directory scanning.
//!
//! ODIM producers ship one file per timestep, with the timestamp
//! encoded in the filename — there is no sidecar index. The catalog's
//! job is to translate a strftime template like
//! `"%Y%m%dT%H%M_radar_fi.h5"` into:
//!
//! 1. A regex that matches just the timestamp portion of the filename
//! 2. A chrono strftime format for parsing the matched substring
//!
//! A scan — local-directory ([`scan_local_directory`]) or S3/HTTP
//! object-store ([`scan_remote`]) — applies the regex to each
//! filename, parses the timestamp, and returns a list of
//! [`CatalogEntry`] values sorted by timestamp ascending.

use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use ds_core::error::DataServerError;
use regex::Regex;
use tracing::warn;

/// Errors from catalog construction and scanning.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("filename_template `{template}` contains no strftime codes — at least one of %Y/%m/%d/%H/%M/%S/%j is required")]
    NoStrftimeCodes { template: String },
    #[error("filename_template `{template}` contains unknown strftime code `{code}`")]
    UnknownCode { template: String, code: String },
    #[error(
        "filename_template `{template}` has non-contiguous strftime codes (more than one block of date/time codes separated by literal text — e.g. `%Y_STATION_%H%M.h5`). \
         The template parser expects all strftime codes to form a single block. Use the explicit `filename_pattern` + `timestamp_format` config form for split layouts."
    )]
    SplitTimestamp { template: String },
    #[error("invalid regex `{pattern}`: {source}")]
    InvalidRegex {
        pattern: String,
        #[source]
        source: regex::Error,
    },
    #[error("filename_pattern `{pattern}` is missing the required `(?P<timestamp>…)` named capture group")]
    NoTimestampCapture { pattern: String },
    #[error("failed to read directory `{dir}`: {source}")]
    ReadDir {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// One file in the catalog, sorted by `time` ascending.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub time: DateTime<Utc>,
    pub location: Location,
}

/// Where one catalog entry's bytes live.
///
/// Local and remote entries are deliberately distinct types, not a
/// shared `PathBuf`: an S3/HTTP object key is **not** a filesystem
/// path. Keys always use `/` regardless of host OS, so round-tripping
/// one through `PathBuf` would corrupt it on a platform with a
/// different separator. A `Remote` location also carries the
/// [`ds_storage::DataStore`] handle (a cheap `Arc` clone) so the
/// entry is self-sufficient — it can be fetched without threading the
/// engine's source state back through every call site.
#[derive(Debug, Clone)]
pub enum Location {
    /// A local filesystem path.
    Local(PathBuf),
    /// An object store plus the full object key within it.
    Remote {
        store: ds_storage::DataStore,
        key: String,
    },
}

impl Location {
    /// Stable identity string — used for cache keying and log /
    /// error messages. For `Local` this is the path; for `Remote`
    /// the object key.
    pub fn id(&self) -> String {
        match self {
            Location::Local(path) => path.display().to_string(),
            Location::Remote { key, .. } => key.clone(),
        }
    }
}

/// Compiled regex + chrono format for matching ODIM filenames and
/// extracting their timestamps. Build once at engine construction
/// time; reuse across every poll.
#[derive(Debug, Clone)]
pub struct FilenameMatcher {
    regex: Regex,
    timestamp_format: String,
}

impl FilenameMatcher {
    /// Build a matcher from a strftime template (e.g.
    /// `"%Y%m%dT%H%M_radar_fi.h5"`). Inverts each known strftime code
    /// to its fixed-width regex equivalent and wraps the contiguous
    /// timestamp region in a `(?P<timestamp>…)` named capture.
    pub fn from_template(template: &str) -> Result<Self, CatalogError> {
        let (body, format) = expand_template(template)?;
        // Anchor templates end-to-end so a trailing `.tmp` partial-
        // upload marker doesn't match. Explicit-regex callers can
        // opt out by adding their own anchors (or not) via
        // `from_pattern`.
        let pattern = format!("^{body}$");
        let regex = Regex::new(&pattern).map_err(|e| CatalogError::InvalidRegex {
            pattern: pattern.clone(),
            source: e,
        })?;
        Ok(Self {
            regex,
            timestamp_format: format,
        })
    }

    /// Build a matcher from an explicit regex + chrono format. Use
    /// this when the producer's filename layout can't be expressed
    /// as a single contiguous strftime template (e.g. when there
    /// are literal `%` characters or interleaved tokens).
    ///
    /// **The pattern is not auto-anchored.** Unlike
    /// [`from_template`], which wraps its regex in `^...$`, this
    /// constructor leaves anchoring to the caller. An unanchored
    /// pattern will match any substring of a filename — including
    /// partial-upload markers such as `radar.h5.tmp` or `radar.h5.part`
    /// — which means `scan_local_directory` will accept the partial
    /// file as a valid catalog entry and could serve a corrupt or
    /// half-written tile as the "latest" timestep. Always include
    /// `^` and `$` in your pattern unless you have a deliberate
    /// reason not to.
    pub fn from_pattern(pattern: &str, timestamp_format: &str) -> Result<Self, CatalogError> {
        if !pattern.contains("(?P<timestamp>") {
            return Err(CatalogError::NoTimestampCapture {
                pattern: pattern.to_string(),
            });
        }
        // Belt-and-suspenders alongside the doc comment: surface a
        // runtime WARN for an operator who reads the config docs
        // but not this constructor's source. `^`/`$` anchors are
        // optional by spec — we don't reject — but most callers
        // want them, and forgetting them silently admits
        // `.tmp`/`.part` partial-upload markers as catalog entries.
        if !pattern.starts_with('^') || !pattern.ends_with('$') {
            warn!(
                "[catalog] filename_pattern `{pattern}` is not fully anchored (`^...$`) — \
                 partial-upload markers like `.tmp` / `.part` may match and be served as \
                 valid catalog entries. Add `^` and `$` to your pattern unless this is \
                 intentional."
            );
        }
        let regex = Regex::new(pattern).map_err(|e| CatalogError::InvalidRegex {
            pattern: pattern.to_string(),
            source: e,
        })?;
        Ok(Self {
            regex,
            timestamp_format: timestamp_format.to_string(),
        })
    }

    /// Try to extract a UTC timestamp from a filename. Returns
    /// `None` if the filename doesn't match the regex or if the
    /// captured substring isn't a valid date under the format.
    pub fn parse_timestamp(&self, filename: &str) -> Option<DateTime<Utc>> {
        let caps = self.regex.captures(filename)?;
        let stamp = caps.name("timestamp")?.as_str();
        NaiveDateTime::parse_from_str(stamp, &self.timestamp_format)
            .ok()
            .map(|t| t.and_utc())
    }
}

/// Walk a local directory, match each filename against `matcher`,
/// and return the matched files sorted by timestamp ascending.
/// `time_filter`, when set, drops entries whose timestamp falls outside
/// the inclusive `(start, end)` range — the local analogue of
/// [`scan_remote`]'s window filter (#465). `max_files` then caps the
/// result at the most recent N; useful when the source directory holds
/// years of history.
///
/// Non-recursive — only files directly in `dir`. Directories,
/// symlinks to directories, and files whose names don't match the
/// matcher are silently skipped.
pub fn scan_local_directory(
    dir: &Path,
    matcher: &FilenameMatcher,
    time_filter: Option<(DateTime<Utc>, DateTime<Utc>)>,
    max_files: Option<usize>,
) -> Result<Vec<CatalogEntry>, CatalogError> {
    let read = std::fs::read_dir(dir).map_err(|e| CatalogError::ReadDir {
        dir: dir.to_path_buf(),
        source: e,
    })?;

    let mut entries = Vec::new();
    for raw in read {
        let raw = match raw {
            Ok(entry) => entry,
            Err(e) => {
                warn!("[catalog] failed to read entry in `{}`: {e}", dir.display());
                continue;
            }
        };
        let path = raw.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(time) = matcher.parse_timestamp(name) {
            entries.push(CatalogEntry {
                time,
                location: Location::Local(path),
            });
        }
    }
    if let Some((start, end)) = time_filter {
        entries.retain(|e| e.time >= start && e.time <= end);
    }
    entries.sort_by_key(|e| e.time);
    if let Some(cap) = max_files {
        if entries.len() > cap {
            let start = entries.len() - cap;
            entries.drain(..start);
        }
    }
    Ok(entries)
}

/// Skip remote objects larger than this — a guard against fetching a
/// mis-uploaded or non-ODIM blob into memory. ODIM COMP composites run
/// from ~35 KB (FMI single-radar) to a few MB (OPERA pan-European);
/// 64 MB is a generous ceiling above any real composite. Enforced both
/// at `list` time ([`scan_remote`]) and at `get` time (the engine's
/// `fetch_bytes`) so an object that grew after listing is still caught.
pub(crate) const MAX_REMOTE_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// Scan an S3/HTTP object store for ODIM files under the given set of
/// (already date-expanded) key prefixes.
///
/// Each prefix is `list`ed; object basenames are matched against
/// `matcher`. Matched entries' `path` holds the full object key — not
/// a filesystem path — which `OdimEngine` resolves back through the
/// object store when loading composites.
///
/// `time_filter`, when set, drops entries whose timestamp falls
/// outside the `(start, end)` range. `max_files` caps the result to
/// the most recent N. Returns entries sorted by timestamp ascending.
///
/// A prefix that fails to `list` (e.g. a date partition that doesn't
/// exist yet) is logged and skipped. If *every* prefix fails the call
/// errors rather than silently returning an empty catalog.
pub fn scan_remote(
    store: &ds_storage::DataStore,
    prefixes: &[String],
    matcher: &FilenameMatcher,
    time_filter: Option<(DateTime<Utc>, DateTime<Utc>)>,
    max_files: Option<usize>,
) -> Result<Vec<CatalogEntry>, DataServerError> {
    use ds_storage::object_store::path::Path as ObjectPath;

    let mut entries: Vec<CatalogEntry> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for prefix in prefixes {
        let listed = match store.list(&ObjectPath::from(prefix.as_str())) {
            Ok(objects) => objects,
            Err(e) => {
                errors.push(format!("'{prefix}': {e}"));
                continue;
            }
        };
        for obj in listed {
            if obj.size > MAX_REMOTE_FILE_SIZE {
                warn!(
                    "[catalog] skipping oversized remote object `{}` ({} bytes)",
                    obj.location, obj.size
                );
                continue;
            }
            let key = obj.location.to_string();
            let name = key.rsplit('/').next().unwrap_or(key.as_str());
            let Some(time) = matcher.parse_timestamp(name) else {
                continue;
            };
            if let Some((start, end)) = time_filter {
                if time < start || time > end {
                    continue;
                }
            }
            entries.push(CatalogEntry {
                time,
                location: Location::Remote {
                    store: store.clone(),
                    key,
                },
            });
        }
    }

    if entries.is_empty() && !errors.is_empty() {
        return Err(DataServerError::Engine(format!(
            "all {} ODIM remote prefix scan(s) failed: {}",
            errors.len(),
            errors.join("; ")
        )));
    }
    if !errors.is_empty() {
        warn!(
            "[catalog] {} ODIM remote prefix scan(s) failed (kept {} entries from the rest): {}",
            errors.len(),
            entries.len(),
            errors.join("; ")
        );
    }

    // Sort by timestamp ascending. Within an equal-timestamp run,
    // order the key *descending* so the lexicographically-greatest key
    // lands first (at the lower array index). `dedup_by` retains the
    // first element of each run — so that greatest key is the one that
    // survives. The sort direction (descending) and the dedup
    // behaviour (keep-first) must stay consistent for this to hold.
    entries.sort_by(|a, b| {
        a.time
            .cmp(&b.time)
            .then_with(|| b.location.id().cmp(&a.location.id()))
    });
    entries.dedup_by(|a, b| a.time == b.time);

    if let Some(cap) = max_files {
        if entries.len() > cap {
            let start = entries.len() - cap;
            entries.drain(..start);
        }
    }
    Ok(entries)
}

/// Convert a strftime template to a regex + chrono format.
///
/// Recognised codes: `%Y` `%m` `%d` `%H` `%M` `%S` `%j` (year,
/// month, day, hour, minute, second, day-of-year). Each maps to a
/// fixed-width digit regex.
///
/// Separator characters within the timestamp region (`T`, `-`, `:`,
/// `_`, `Z`) are kept inside the named capture group so they round-
/// trip through `parse_from_str` correctly.
fn expand_template(template: &str) -> Result<(String, String), CatalogError> {
    const CODES: &[(&str, &str)] = &[
        ("%Y", r"\d{4}"),
        ("%m", r"\d{2}"),
        ("%d", r"\d{2}"),
        ("%H", r"\d{2}"),
        ("%M", r"\d{2}"),
        ("%S", r"\d{2}"),
        ("%j", r"\d{3}"),
    ];

    let mut regex = String::new();
    let mut format = String::new();
    let mut in_timestamp = false;
    let mut timestamp_started = false;
    let mut timestamp_closed = false;
    let bytes = template.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            let mut matched = false;
            for &(code, re) in CODES {
                if template[i..].starts_with(code) {
                    if !in_timestamp {
                        // Re-entering the timestamp region after it
                        // was already closed = split layout. Emit a
                        // dedicated error rather than letting `Regex::new`
                        // reject the "duplicate named capture group"
                        // with an opaque message.
                        if timestamp_closed {
                            return Err(CatalogError::SplitTimestamp {
                                template: template.to_string(),
                            });
                        }
                        regex.push_str("(?P<timestamp>");
                        in_timestamp = true;
                    }
                    timestamp_started = true;
                    format.push_str(code);
                    regex.push_str(re);
                    i += code.len();
                    matched = true;
                    break;
                }
            }
            if !matched {
                let code = std::str::from_utf8(&bytes[i..i + 2.min(bytes.len() - i)])
                    .unwrap_or("??")
                    .to_string();
                return Err(CatalogError::UnknownCode {
                    template: template.to_string(),
                    code,
                });
            }
        } else {
            let ch = bytes[i] as char;
            if in_timestamp {
                let is_separator = matches!(ch, 'T' | '-' | ':' | '_' | 'Z');
                let next_is_code = i + 2 < bytes.len()
                    && bytes[i + 1] == b'%'
                    && CODES.iter().any(|(c, _)| template[i + 1..].starts_with(c));
                if is_separator && next_is_code {
                    format.push(ch);
                    regex.push_str(&regex::escape(&ch.to_string()));
                    i += 1;
                } else if ch == 'Z' && !next_is_code {
                    format.push('Z');
                    regex.push('Z');
                    regex.push(')');
                    in_timestamp = false;
                    timestamp_closed = true;
                    i += 1;
                } else {
                    regex.push(')');
                    in_timestamp = false;
                    timestamp_closed = true;
                    regex.push_str(&regex::escape(&ch.to_string()));
                    i += 1;
                }
            } else {
                regex.push_str(&regex::escape(&ch.to_string()));
                i += 1;
            }
        }
    }

    if in_timestamp {
        regex.push(')');
    }

    if !timestamp_started {
        return Err(CatalogError::NoStrftimeCodes {
            template: template.to_string(),
        });
    }

    Ok((regex, format))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmi_polar_composite_template_matches_canonical_filename() {
        let m = FilenameMatcher::from_template("%Y%m%dT%H%M_radar_fi.h5").unwrap();
        let ts = m.parse_timestamp("20250714T1530_radar_fi.h5").unwrap();
        assert_eq!(ts.to_rfc3339(), "2025-07-14T15:30:00+00:00");
    }

    #[test]
    fn dmi_underscore_separator_template() {
        let m = FilenameMatcher::from_template("comp_%Y_%m_%d_%H%M.h5").unwrap();
        let ts = m.parse_timestamp("comp_2025_07_14_1530.h5").unwrap();
        assert_eq!(ts.to_rfc3339(), "2025-07-14T15:30:00+00:00");
    }

    /// OPERA composites use `@` to delimit the timestamp from
    /// surrounding tokens (product/version/quantity). `@` isn't in
    /// the explicit separator set, so it falls into the general
    /// "close timestamp group and emit literal" branch — pinning
    /// the expected behaviour here guards against an `expand_template`
    /// refactor silently breaking OPERA.
    #[test]
    fn opera_at_separator_template() {
        let m = FilenameMatcher::from_template("OPERA@%Y%m%dT%H%M@0@DBZH.h5").unwrap();
        let ts = m.parse_timestamp("OPERA@20250714T1530@0@DBZH.h5").unwrap();
        assert_eq!(ts.to_rfc3339(), "2025-07-14T15:30:00+00:00");
        assert_eq!(m.parse_timestamp("OPERA@20250714T1530@0@DBZH.h5.tmp"), None);
    }

    /// Trailing `Z` (UTC marker) is part of the timestamp region.
    /// Without this, the matcher would close the capture group
    /// before the `Z` and the chrono parser would reject `2025…30`
    /// against a format ending in `Z`.
    #[test]
    fn trailing_z_is_part_of_the_timestamp() {
        let m = FilenameMatcher::from_template("radar_%Y%m%dT%H%MZ.h5").unwrap();
        let ts = m.parse_timestamp("radar_20250714T1530Z.h5").unwrap();
        assert_eq!(ts.to_rfc3339(), "2025-07-14T15:30:00+00:00");
    }

    /// A template with two disjoint strftime blocks (e.g.
    /// `%Y_STATION_%H%M.h5`) would emit two `(?P<timestamp>...)`
    /// regex groups, which the `regex` crate rejects with an opaque
    /// "duplicate named capture group" error. We catch this at
    /// expand time with a dedicated `SplitTimestamp` variant so the
    /// operator gets a clear hint to use the explicit
    /// `filename_pattern` + `timestamp_format` form instead.
    #[test]
    fn split_timestamp_template_is_an_error() {
        let err = FilenameMatcher::from_template("%Y_STATION_%H%M.h5").unwrap_err();
        match err {
            CatalogError::SplitTimestamp { ref template } => {
                assert_eq!(template, "%Y_STATION_%H%M.h5");
            }
            other => panic!("expected SplitTimestamp, got {other:?}"),
        }
    }

    /// Filenames that don't match the regex should return `None`,
    /// not error — the directory scan skips them silently because
    /// real radar directories often contain log files, lock files,
    /// or `.tmp` partial-upload markers.
    #[test]
    fn unrelated_filenames_return_none() {
        let m = FilenameMatcher::from_template("%Y%m%dT%H%M_radar_fi.h5").unwrap();
        assert_eq!(m.parse_timestamp("README.md"), None);
        assert_eq!(m.parse_timestamp("20250714T1530_radar_fi.h5.tmp"), None);
        assert_eq!(m.parse_timestamp("malformed_radar_fi.h5"), None);
    }

    #[test]
    fn template_without_strftime_codes_is_an_error() {
        let err = FilenameMatcher::from_template("radar.h5").unwrap_err();
        match err {
            CatalogError::NoStrftimeCodes { .. } => {}
            other => panic!("expected NoStrftimeCodes, got {other:?}"),
        }
    }

    #[test]
    fn unknown_strftime_code_is_an_error() {
        let err = FilenameMatcher::from_template("radar_%X.h5").unwrap_err();
        match err {
            CatalogError::UnknownCode { code, .. } => assert_eq!(code, "%X"),
            other => panic!("expected UnknownCode, got {other:?}"),
        }
    }

    /// Explicit-regex path: when the filename layout can't be
    /// expressed as a contiguous strftime template, the operator
    /// can supply a regex + format pair directly.
    #[test]
    fn explicit_pattern_with_named_capture_group() {
        let m = FilenameMatcher::from_pattern(r"^comp-(?P<timestamp>\d{12})\.h5$", "%Y%m%d%H%M")
            .unwrap();
        let ts = m.parse_timestamp("comp-202507141530.h5").unwrap();
        assert_eq!(ts.to_rfc3339(), "2025-07-14T15:30:00+00:00");
    }

    #[test]
    fn explicit_pattern_without_timestamp_capture_is_an_error() {
        let err = FilenameMatcher::from_pattern(r"^comp-(\d+)\.h5$", "%Y%m%d%H%M").unwrap_err();
        assert!(matches!(err, CatalogError::NoTimestampCapture { .. }));
    }

    #[test]
    fn scan_local_directory_sorts_by_timestamp_ascending() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "20250714T1500_radar_fi.h5",
            "20250714T1530_radar_fi.h5",
            "20250714T1400_radar_fi.h5",
            "README.md",
        ] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let m = FilenameMatcher::from_template("%Y%m%dT%H%M_radar_fi.h5").unwrap();
        let entries = scan_local_directory(dir.path(), &m, None, None).unwrap();
        assert_eq!(entries.len(), 3, "README.md must be skipped");
        let times: Vec<_> = entries.iter().map(|e| e.time.to_rfc3339()).collect();
        assert_eq!(
            times,
            [
                "2025-07-14T14:00:00+00:00",
                "2025-07-14T15:00:00+00:00",
                "2025-07-14T15:30:00+00:00",
            ]
        );
    }

    /// `time_filter` drops entries outside the inclusive window (before
    /// the `max_files` cap) — a local source with `time_window` retains
    /// only the recent tail even when the directory holds more history
    /// (#465; previously the window was silently ignored for local
    /// sources).
    #[test]
    fn scan_local_directory_applies_time_filter() {
        let dir = tempfile::tempdir().unwrap();
        for hour in 0..5 {
            let name = format!("20250714T{hour:02}00_radar_fi.h5");
            std::fs::write(dir.path().join(&name), b"").unwrap();
        }
        let m = FilenameMatcher::from_template("%Y%m%dT%H%M_radar_fi.h5").unwrap();
        let start: DateTime<Utc> = "2025-07-14T02:00:00Z".parse().unwrap();
        let end: DateTime<Utc> = "2025-07-14T03:00:00Z".parse().unwrap();
        let entries = scan_local_directory(dir.path(), &m, Some((start, end)), None).unwrap();
        let times: Vec<_> = entries.iter().map(|e| e.time.to_rfc3339()).collect();
        assert_eq!(
            times,
            ["2025-07-14T02:00:00+00:00", "2025-07-14T03:00:00+00:00"],
            "window is inclusive on both ends; entries outside are dropped"
        );
    }

    /// `max_files` keeps the most recent N. A directory holding
    /// years of history shouldn't blow the catalog up; the engine
    /// only ever needs the recent tail.
    #[test]
    fn scan_local_directory_respects_max_files_cap() {
        let dir = tempfile::tempdir().unwrap();
        for hour in 0..5 {
            let name = format!("20250714T{hour:02}00_radar_fi.h5");
            std::fs::write(dir.path().join(&name), b"").unwrap();
        }
        let m = FilenameMatcher::from_template("%Y%m%dT%H%M_radar_fi.h5").unwrap();
        let entries = scan_local_directory(dir.path(), &m, None, Some(2)).unwrap();
        assert_eq!(entries.len(), 2);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.location.id().rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "20250714T0300_radar_fi.h5".to_string(),
                "20250714T0400_radar_fi.h5".to_string(),
            ]
        );
    }

    /// `scan_remote` against a `DataStore` backed by the local
    /// filesystem — exercises the list → match → filter → cap pipeline
    /// without needing a live S3 endpoint. The `LocalFileSystem`
    /// object store behaves like S3 for `list`, so this covers the
    /// real remote code path.
    #[test]
    fn scan_remote_matches_filters_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "OPERA@20260515T0000@0@DBZH.h5",
            "OPERA@20260515T0005@0@DBZH.h5",
            "OPERA@20260515T0010@0@DBZH.h5",
            "OPERA@20260515T0015@0@DBZH.h5",
            "README.md",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let (store, _) = ds_storage::build_store(dir.path().to_str().unwrap()).unwrap();
        let matcher = FilenameMatcher::from_template("OPERA@%Y%m%dT%H%M@0@DBZH.h5").unwrap();

        // max_files caps to the most recent 2; README is skipped.
        let capped = scan_remote(&store, &["".to_string()], &matcher, None, Some(2)).unwrap();
        let times: Vec<_> = capped.iter().map(|e| e.time.to_rfc3339()).collect();
        assert_eq!(
            times,
            ["2026-05-15T00:10:00+00:00", "2026-05-15T00:15:00+00:00"]
        );

        // time_filter drops entries outside the [00:05, 00:10] range.
        let start = "2026-05-15T00:05:00Z".parse().unwrap();
        let end = "2026-05-15T00:10:00Z".parse().unwrap();
        let windowed = scan_remote(
            &store,
            &["".to_string()],
            &matcher,
            Some((start, end)),
            None,
        )
        .unwrap();
        let times: Vec<_> = windowed.iter().map(|e| e.time.to_rfc3339()).collect();
        assert_eq!(
            times,
            ["2026-05-15T00:05:00+00:00", "2026-05-15T00:10:00+00:00"]
        );
    }

    /// Two objects under different prefixes share a timestamp. `dedup`
    /// must collapse them to exactly one entry, deterministically
    /// keeping the lexicographically-last key. Pins the tie-break so a
    /// future change to the sort predicate can't silently flip the
    /// winner.
    #[test]
    fn scan_remote_dedups_duplicate_timestamps_keeping_last_key() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["a", "b"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
            std::fs::write(
                dir.path().join(sub).join("OPERA@20260515T0000@0@DBZH.h5"),
                b"x",
            )
            .unwrap();
        }
        let (store, _) = ds_storage::build_store(dir.path().to_str().unwrap()).unwrap();
        let matcher = FilenameMatcher::from_template("OPERA@%Y%m%dT%H%M@0@DBZH.h5").unwrap();

        let entries = scan_remote(
            &store,
            &["a".to_string(), "b".to_string()],
            &matcher,
            None,
            None,
        )
        .unwrap();
        assert_eq!(entries.len(), 1, "duplicate timestamp must collapse to one");
        assert_eq!(entries[0].location.id(), "b/OPERA@20260515T0000@0@DBZH.h5");
    }

    /// Entries from multiple prefixes are merged; a prefix that yields
    /// nothing (or fails to `list`) doesn't sink the scan as long as
    /// another prefix produces entries.
    #[test]
    fn scan_remote_merges_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("good")).unwrap();
        std::fs::write(dir.path().join("good/OPERA@20260515T0000@0@DBZH.h5"), b"x").unwrap();
        let (store, _) = ds_storage::build_store(dir.path().to_str().unwrap()).unwrap();
        let matcher = FilenameMatcher::from_template("OPERA@%Y%m%dT%H%M@0@DBZH.h5").unwrap();

        let entries = scan_remote(
            &store,
            &["good".to_string(), "missing".to_string()],
            &matcher,
            None,
            None,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].time.to_rfc3339(), "2026-05-15T00:00:00+00:00");
    }
}
