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
//! Then a local-directory scan walks the directory, applies the regex
//! to each filename, parses the timestamp, and returns a sorted list
//! of `(timestamp, path)` entries.
//!
//! Phase 1 scope is local directories only. S3-backed catalogs land
//! in Phase 1.5 once the engine wiring is proven end-to-end (see
//! [[project_odim_engine_plan]]).

use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use regex::Regex;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub time: DateTime<Utc>,
    pub path: PathBuf,
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
    pub fn from_pattern(pattern: &str, timestamp_format: &str) -> Result<Self, CatalogError> {
        if !pattern.contains("(?P<timestamp>") {
            return Err(CatalogError::NoTimestampCapture {
                pattern: pattern.to_string(),
            });
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
/// and return the matched files sorted by timestamp ascending. Caps
/// the returned vec at `max_files` from the end (most recent) when
/// set; useful when the source directory holds years of history.
///
/// Non-recursive — only files directly in `dir`. Directories,
/// symlinks to directories, and files whose names don't match the
/// matcher are silently skipped.
pub fn scan_local_directory(
    dir: &Path,
    matcher: &FilenameMatcher,
    max_files: Option<usize>,
) -> Result<Vec<CatalogEntry>, CatalogError> {
    let read = std::fs::read_dir(dir).map_err(|e| CatalogError::ReadDir {
        dir: dir.to_path_buf(),
        source: e,
    })?;

    let mut entries = Vec::new();
    for raw in read.flatten() {
        let path = raw.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(time) = matcher.parse_timestamp(name) {
            entries.push(CatalogEntry { time, path });
        }
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
        let entries = scan_local_directory(dir.path(), &m, None).unwrap();
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
        let entries = scan_local_directory(dir.path(), &m, Some(2)).unwrap();
        assert_eq!(entries.len(), 2);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "20250714T0300_radar_fi.h5".to_string(),
                "20250714T0400_radar_fi.h5".to_string(),
            ]
        );
    }
}
