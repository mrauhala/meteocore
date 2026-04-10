//! Parser for NOAA wgrib2-style GRIB2 index files (.idx).
//!
//! Format: colon-separated text, one message per line:
//!   record_num:byte_offset:d=YYYYMMDDHH:PARAM:level_descriptor:time_descriptor:
//!
//! Unlike ECMWF JSON indexes, wgrib2 indexes:
//!   - cover a single forecast step per file
//!   - carry only offsets (lengths must be computed from the next offset, or
//!     from a HEAD request for the last record)
//!   - use text level descriptors and time descriptors that require parsing

#![allow(dead_code)]

use chrono::{DateTime, TimeZone, Utc};

/// Maximum plausible GRIB2 message length. Anything larger indicates a
/// corrupted index or overflowing offsets, so we reject the whole file.
const MAX_MESSAGE_LEN: u64 = 1 << 30; // 1 GiB

/// Forecast step kind. v1 distinguishes instantaneous values from max/min
/// aggregates (coerced to the window end) and drops accumulation and average
/// aggregates entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Instantaneous forecast at `nominal_step` hours.
    Instant,
    /// Maximum over the window [start, end]; we coerce to nominal_step = end.
    MaxOverWindow { start: u32, end: u32 },
    /// Minimum over the window; we coerce to nominal_step = end.
    MinOverWindow { start: u32, end: u32 },
}

/// One parsed message. Length is `None` for the last record in a file
/// (resolved later by the engine from the data file size).
#[derive(Debug, Clone)]
pub struct ParsedMessage {
    pub short_name: String,
    pub levtype: &'static str,
    pub level: Option<u32>,
    pub offset: u64,
    pub length: Option<u64>,
    pub step_kind: StepKind,
    pub nominal_step: u32,
}

/// Result of parsing a wgrib2 index file.
#[derive(Debug, Clone)]
pub struct WgribIndexResult {
    pub reference_time: DateTime<Utc>,
    pub messages: Vec<ParsedMessage>,
}

/// Canonical level type after mapping wgrib2 text descriptors.
///
/// Returns `(canonical_levtype, Option<level_value>)` or `None` if the
/// descriptor is not recognised.
pub fn level_desc_to_canonical(desc: &str) -> Option<(&'static str, Option<u32>)> {
    let lower = desc.trim().to_ascii_lowercase();

    // Surface-like aliases with no numeric level.
    match lower.as_str() {
        "surface" => return Some(("sfc", None)),
        "mean sea level" => return Some(("sfc", None)),
        "entire atmosphere" => return Some(("sfc", None)),
        "entire atmosphere (considered as a single layer)" => return Some(("sfc", None)),
        "tropopause" => return Some(("sfc", None)),
        "max wind" => return Some(("sfc", None)),
        "convective cloud bottom level" => return Some(("sfc", None)),
        "convective cloud top level" => return Some(("sfc", None)),
        "convective cloud layer" => return Some(("sfc", None)),
        "planetary boundary layer" => return Some(("sfc", None)),
        "cloud ceiling" => return Some(("sfc", None)),
        "0c isotherm" => return Some(("sfc", None)),
        "highest tropospheric freezing level" => return Some(("sfc", None)),
        _ => {}
    }

    // Try patterns with a numeric prefix.
    // "{N} m above ground"
    if let Some(rest) = lower.strip_suffix(" m above ground") {
        if let Some(n) = parse_nonneg_float_to_u32(rest) {
            return Some(("hag", Some(n)));
        }
        return None;
    }

    // "{N} mb"
    if let Some(rest) = lower.strip_suffix(" mb") {
        if let Some(n) = parse_nonneg_float_to_u32(rest) {
            return Some(("pl", Some(n)));
        }
        return None;
    }

    // "{N} hybrid level"
    if let Some(rest) = lower.strip_suffix(" hybrid level") {
        if let Some(n) = parse_nonneg_float_to_u32(rest) {
            return Some(("ml", Some(n)));
        }
        return None;
    }

    // "{N} sigma level"
    if let Some(rest) = lower.strip_suffix(" sigma level") {
        if let Some(n) = parse_nonneg_float_to_u32(rest) {
            return Some(("ml", Some(n)));
        }
        return None;
    }

    // "{N} K level" (isentropic). Use case-insensitive match on the "K".
    if let Some(rest) = lower.strip_suffix(" k level") {
        if let Some(n) = parse_nonneg_float_to_u32(rest) {
            return Some(("iso", Some(n)));
        }
        return None;
    }

    // "{N}-{M} m below ground"
    if let Some(rest) = lower.strip_suffix(" m below ground") {
        // rest is "{N}-{M}"
        if let Some((a, _b)) = rest.split_once('-') {
            if let Some(n) = parse_nonneg_float_floor_u32(a) {
                return Some(("sol", Some(n)));
            }
        }
        return None;
    }

    None
}

/// Parse a non-negative float and round to u32. Returns None if the value
/// is fractional in a way that loses precision unacceptably, or cannot be
/// parsed, or is negative.
fn parse_nonneg_float_to_u32(s: &str) -> Option<u32> {
    let s = s.trim();
    // Integer fast path.
    if let Ok(n) = s.parse::<u32>() {
        return Some(n);
    }
    // Float path: accept decimals that are effectively integers (e.g. "2.0").
    let f: f64 = s.parse().ok()?;
    if !f.is_finite() || f < 0.0 || f > u32::MAX as f64 {
        return None;
    }
    let rounded = f.round();
    if (f - rounded).abs() < 1e-6 {
        return Some(rounded as u32);
    }
    // Fractional value that does not round cleanly: skip this record.
    None
}

/// Parse a non-negative numeric value and floor it to u32. Used for
/// "{N}-{M} m below ground" where depths may be decimal.
fn parse_nonneg_float_floor_u32(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u32>() {
        return Some(n);
    }
    let f: f64 = s.parse().ok()?;
    if !f.is_finite() || f < 0.0 || f > u32::MAX as f64 {
        return None;
    }
    Some(f.floor() as u32)
}

/// Parse a "d=YYYYMMDDHH" reference-time token.
pub fn parse_wgrib2_ref_time(token: &str) -> Option<DateTime<Utc>> {
    let digits = token.strip_prefix("d=")?;
    if digits.len() != 10 {
        return None;
    }
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let year: i32 = digits[0..4].parse().ok()?;
    let month: u32 = digits[4..6].parse().ok()?;
    let day: u32 = digits[6..8].parse().ok()?;
    let hour: u32 = digits[8..10].parse().ok()?;

    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).single()
}

/// Parse a wgrib2 time descriptor.
///
/// Returns `Some((nominal_step_hours, StepKind))` for instantaneous and
/// max/min fields; `None` for `acc`, `ave`, and anything else we skip in v1.
pub fn parse_wgrib2_step(desc: &str) -> Option<(u32, StepKind)> {
    let lower = desc.trim().to_ascii_lowercase();

    if lower == "anl" {
        return Some((0, StepKind::Instant));
    }

    // Check longer/more-specific suffixes first so that e.g.
    // "0-6 hour min fcst" doesn't accidentally match " min fcst".

    // "{M}-{N} hour max fcst"
    if let Some(rest) = lower.strip_suffix(" hour max fcst") {
        let (m_str, n_str) = rest.split_once('-')?;
        let m: u32 = m_str.trim().parse().ok()?;
        let n: u32 = n_str.trim().parse().ok()?;
        return Some((n, StepKind::MaxOverWindow { start: m, end: n }));
    }

    // "{M}-{N} hour min fcst"
    if let Some(rest) = lower.strip_suffix(" hour min fcst") {
        let (m_str, n_str) = rest.split_once('-')?;
        let m: u32 = m_str.trim().parse().ok()?;
        let n: u32 = n_str.trim().parse().ok()?;
        return Some((n, StepKind::MinOverWindow { start: m, end: n }));
    }

    // Dropped in v1: accumulations and averages.
    if lower.ends_with(" hour acc fcst") || lower.ends_with(" hour ave fcst") {
        return None;
    }

    // "{N} hour fcst"
    if let Some(rest) = lower.strip_suffix(" hour fcst") {
        if rest.contains('-') {
            return None;
        }
        let n: u32 = rest.trim().parse().ok()?;
        return Some((n, StepKind::Instant));
    }

    // "{N} min fcst" — only matches when "min" is not preceded by another word.
    if let Some(rest) = lower.strip_suffix(" min fcst") {
        if rest.contains('-') || rest.contains(' ') {
            return None;
        }
        let n: u32 = rest.trim().parse().ok()?;
        return Some((n / 60, StepKind::Instant));
    }

    None
}

/// Parse the contents of a wgrib2 `.idx` file.
///
/// Returns `None` if the input is empty, if no messages could be parsed, or
/// if the reference times across records are inconsistent.
///
/// On success, messages are sorted by ascending byte offset, and all but the
/// last record have a concrete length computed from the next-record offset.
pub fn parse_wgrib2(content: &str) -> Option<WgribIndexResult> {
    let mut reference_time: Option<DateTime<Utc>> = None;
    let mut records: Vec<(u32, ParsedMessage)> = Vec::new();

    for (line_no, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            continue;
        }

        // Split into at most 6 fields. After 5 colon splits, the 6th
        // (last) field absorbs any remaining colons, so a time descriptor
        // that itself contains colons is preserved intact. Lines with a
        // trailing colon leave that colon on parts[5]; we strip it below.
        let parts: Vec<&str> = line.splitn(6, ':').collect();
        if parts.len() < 6 {
            tracing::debug!(
                "wgrib2_index: skipping malformed line {} (got {} fields): {}",
                line_no + 1,
                parts.len(),
                line
            );
            continue;
        }

        let record_num_str = parts[0].trim();
        let offset_str = parts[1].trim();
        let ref_time_token = parts[2].trim();
        let param_short_name = parts[3].trim();
        let level_desc = parts[4].trim();
        // The last field may carry the terminating ":" from the line. Trim
        // trailing colons (but not internal ones — see splitn above).
        let time_desc = parts[5].trim_end_matches(':').trim();

        let record_num: u32 = match record_num_str.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::debug!(
                    "wgrib2_index: skipping line {}: bad record number {:?}",
                    line_no + 1,
                    record_num_str
                );
                continue;
            }
        };

        let byte_offset: u64 = match offset_str.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::debug!(
                    "wgrib2_index: skipping line {}: bad byte offset {:?}",
                    line_no + 1,
                    offset_str
                );
                continue;
            }
        };

        let parsed_ref = match parse_wgrib2_ref_time(ref_time_token) {
            Some(t) => t,
            None => {
                tracing::debug!(
                    "wgrib2_index: skipping line {}: bad reference time {:?}",
                    line_no + 1,
                    ref_time_token
                );
                continue;
            }
        };

        match reference_time {
            None => reference_time = Some(parsed_ref),
            Some(existing) if existing != parsed_ref => {
                tracing::warn!(
                    "wgrib2_index: inconsistent reference times ({} vs {}), rejecting file",
                    existing,
                    parsed_ref
                );
                return None;
            }
            _ => {}
        }

        let (levtype, level) = match level_desc_to_canonical(level_desc) {
            Some(v) => v,
            None => {
                tracing::debug!(
                    "wgrib2_index: skipping line {}: unknown level descriptor {:?}",
                    line_no + 1,
                    level_desc
                );
                continue;
            }
        };

        let (nominal_step, step_kind) = match parse_wgrib2_step(time_desc) {
            Some(v) => v,
            None => {
                tracing::debug!(
                    "wgrib2_index: skipping line {}: unsupported time descriptor {:?}",
                    line_no + 1,
                    time_desc
                );
                continue;
            }
        };

        records.push((
            record_num,
            ParsedMessage {
                short_name: param_short_name.to_string(),
                levtype,
                level,
                offset: byte_offset,
                length: None,
                step_kind,
                nominal_step,
            },
        ));
    }

    let reference_time = reference_time?;
    if records.is_empty() {
        return None;
    }

    // Sort by offset ascending. wgrib2 normally emits records in order, but
    // we do not rely on that.
    records.sort_by_key(|(_, m)| m.offset);

    // Deduplicate by offset: if two records share the same offset, keep the
    // first and warn. This also prevents zero-length entries from appearing
    // in the length-computation step.
    let mut deduped: Vec<(u32, ParsedMessage)> = Vec::with_capacity(records.len());
    for rec in records.into_iter() {
        if let Some(last) = deduped.last() {
            if last.1.offset == rec.1.offset {
                tracing::warn!(
                    "wgrib2_index: duplicate byte offset {} (records {} and {}), keeping first",
                    rec.1.offset,
                    last.0,
                    rec.0
                );
                continue;
            }
        }
        deduped.push(rec);
    }

    if deduped.is_empty() {
        return None;
    }

    // Compute lengths from successive offsets.
    let n = deduped.len();
    for i in 0..n.saturating_sub(1) {
        let this_offset = deduped[i].1.offset;
        let next_offset = deduped[i + 1].1.offset;
        let length = next_offset.saturating_sub(this_offset);
        if length == 0 {
            // Should have been caught by dedup, but guard anyway.
            tracing::warn!(
                "wgrib2_index: zero-length message at offset {}, rejecting file",
                this_offset
            );
            return None;
        }
        if length > MAX_MESSAGE_LEN {
            tracing::warn!(
                "wgrib2_index: implausible message length {} at offset {}, rejecting file",
                length,
                this_offset
            );
            return None;
        }
        deduped[i].1.length = Some(length);
    }
    // The last record's length stays None and will be resolved later by
    // the engine via a HEAD request on the data file.

    let messages: Vec<ParsedMessage> = deduped.into_iter().map(|(_, m)| m).collect();

    if messages.is_empty() {
        return None;
    }

    Some(WgribIndexResult {
        reference_time,
        messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- Level descriptor tests ----------

    #[test]
    fn level_surface_aliases() {
        assert_eq!(level_desc_to_canonical("surface"), Some(("sfc", None)));
        assert_eq!(
            level_desc_to_canonical("mean sea level"),
            Some(("sfc", None))
        );
        assert_eq!(
            level_desc_to_canonical("entire atmosphere"),
            Some(("sfc", None))
        );
        assert_eq!(
            level_desc_to_canonical("entire atmosphere (considered as a single layer)"),
            Some(("sfc", None))
        );
        assert_eq!(level_desc_to_canonical("tropopause"), Some(("sfc", None)));
        assert_eq!(level_desc_to_canonical("max wind"), Some(("sfc", None)));
        assert_eq!(
            level_desc_to_canonical("convective cloud top level"),
            Some(("sfc", None))
        );
        assert_eq!(
            level_desc_to_canonical("convective cloud bottom level"),
            Some(("sfc", None))
        );
        assert_eq!(
            level_desc_to_canonical("convective cloud layer"),
            Some(("sfc", None))
        );
        assert_eq!(
            level_desc_to_canonical("planetary boundary layer"),
            Some(("sfc", None))
        );
        assert_eq!(
            level_desc_to_canonical("cloud ceiling"),
            Some(("sfc", None))
        );
        assert_eq!(level_desc_to_canonical("0C isotherm"), Some(("sfc", None)));
        assert_eq!(
            level_desc_to_canonical("highest tropospheric freezing level"),
            Some(("sfc", None))
        );
    }

    #[test]
    fn level_height_above_ground() {
        assert_eq!(
            level_desc_to_canonical("2 m above ground"),
            Some(("hag", Some(2)))
        );
        assert_eq!(
            level_desc_to_canonical("10 m above ground"),
            Some(("hag", Some(10)))
        );
        assert_eq!(
            level_desc_to_canonical("80 m above ground"),
            Some(("hag", Some(80)))
        );
        assert_eq!(
            level_desc_to_canonical("100 m above ground"),
            Some(("hag", Some(100)))
        );
    }

    #[test]
    fn level_pressure_levels() {
        assert_eq!(level_desc_to_canonical("500 mb"), Some(("pl", Some(500))));
        assert_eq!(level_desc_to_canonical("850 mb"), Some(("pl", Some(850))));
    }

    #[test]
    fn level_model_levels() {
        assert_eq!(
            level_desc_to_canonical("1 hybrid level"),
            Some(("ml", Some(1)))
        );
        assert_eq!(
            level_desc_to_canonical("20 hybrid level"),
            Some(("ml", Some(20)))
        );
        assert_eq!(
            level_desc_to_canonical("5 sigma level"),
            Some(("ml", Some(5)))
        );
    }

    #[test]
    fn level_below_ground() {
        // For "0-0.1 m below ground" we take the shallower (first) depth,
        // which is 0 — so we return ("sol", Some(0)). This matches the
        // spec: "use the shallower depth".
        assert_eq!(
            level_desc_to_canonical("0-0.1 m below ground"),
            Some(("sol", Some(0)))
        );
        // For "0.1-0.4 m below ground", the shallower depth is 0.1 m;
        // we floor it to 0 cm in integer metres. Document this: the parser
        // currently loses sub-metre resolution, which is acceptable for v1
        // because GFS soil layers are typically aliased by index order.
        assert_eq!(
            level_desc_to_canonical("0.1-0.4 m below ground"),
            Some(("sol", Some(0)))
        );
        // A clearly integer layer.
        assert_eq!(
            level_desc_to_canonical("1-2 m below ground"),
            Some(("sol", Some(1)))
        );
    }

    #[test]
    fn level_isentropic() {
        assert_eq!(
            level_desc_to_canonical("315 K level"),
            Some(("iso", Some(315)))
        );
    }

    #[test]
    fn level_unknown() {
        assert_eq!(level_desc_to_canonical("gobbledygook"), None);
        assert_eq!(level_desc_to_canonical(""), None);
        assert_eq!(level_desc_to_canonical("500"), None);
    }

    // ---------- Reference time tests ----------

    #[test]
    fn ref_time_parses_hour_00() {
        let t = parse_wgrib2_ref_time("d=2026040800").unwrap();
        assert_eq!(t, Utc.with_ymd_and_hms(2026, 4, 8, 0, 0, 0).unwrap());
    }

    #[test]
    fn ref_time_parses_hour_12() {
        let t = parse_wgrib2_ref_time("d=2026040812").unwrap();
        assert_eq!(t, Utc.with_ymd_and_hms(2026, 4, 8, 12, 0, 0).unwrap());
    }

    #[test]
    fn ref_time_rejects_short() {
        assert!(parse_wgrib2_ref_time("d=20260408").is_none());
    }

    #[test]
    fn ref_time_rejects_missing_prefix() {
        assert!(parse_wgrib2_ref_time("20260408").is_none());
        assert!(parse_wgrib2_ref_time("2026040800").is_none());
    }

    #[test]
    fn ref_time_rejects_non_digits() {
        assert!(parse_wgrib2_ref_time("d=YYYYMMDDHH").is_none());
        assert!(parse_wgrib2_ref_time("d=20260408XX").is_none());
    }

    // ---------- Time descriptor tests ----------

    #[test]
    fn step_anl() {
        assert_eq!(parse_wgrib2_step("anl"), Some((0, StepKind::Instant)));
    }

    #[test]
    fn step_hour_fcst() {
        assert_eq!(
            parse_wgrib2_step("6 hour fcst"),
            Some((6, StepKind::Instant))
        );
        assert_eq!(
            parse_wgrib2_step("12 hour fcst"),
            Some((12, StepKind::Instant))
        );
        assert_eq!(
            parse_wgrib2_step("120 hour fcst"),
            Some((120, StepKind::Instant))
        );
    }

    #[test]
    fn step_min_fcst() {
        // 120 min -> 2 hours.
        assert_eq!(
            parse_wgrib2_step("120 min fcst"),
            Some((2, StepKind::Instant))
        );
        // 30 min -> 0 hours (integer division).
        assert_eq!(
            parse_wgrib2_step("30 min fcst"),
            Some((0, StepKind::Instant))
        );
    }

    #[test]
    fn step_max_window() {
        assert_eq!(
            parse_wgrib2_step("0-6 hour max fcst"),
            Some((6, StepKind::MaxOverWindow { start: 0, end: 6 }))
        );
        assert_eq!(
            parse_wgrib2_step("6-12 hour max fcst"),
            Some((12, StepKind::MaxOverWindow { start: 6, end: 12 }))
        );
    }

    #[test]
    fn step_min_window() {
        assert_eq!(
            parse_wgrib2_step("0-6 hour min fcst"),
            Some((6, StepKind::MinOverWindow { start: 0, end: 6 }))
        );
    }

    #[test]
    fn step_acc_dropped() {
        assert_eq!(parse_wgrib2_step("0-6 hour acc fcst"), None);
        assert_eq!(parse_wgrib2_step("6-12 hour acc fcst"), None);
    }

    #[test]
    fn step_ave_dropped() {
        assert_eq!(parse_wgrib2_step("0-6 hour ave fcst"), None);
    }

    #[test]
    fn step_unknown_dropped() {
        assert_eq!(parse_wgrib2_step(""), None);
        assert_eq!(parse_wgrib2_step("gibberish"), None);
        assert_eq!(parse_wgrib2_step("7 day fcst"), None);
    }

    // ---------- Full-file happy-path tests ----------

    #[test]
    fn parses_three_record_happy_path() {
        let input = "\
1:0:d=2026040800:TMP:2 m above ground:anl:
2:1000:d=2026040800:TMP:2 m above ground:6 hour fcst:
3:3000:d=2026040800:TMP:2 m above ground:12 hour fcst:
";
        let result = parse_wgrib2(input).expect("should parse");
        assert_eq!(
            result.reference_time,
            Utc.with_ymd_and_hms(2026, 4, 8, 0, 0, 0).unwrap()
        );
        assert_eq!(result.messages.len(), 3);

        assert_eq!(result.messages[0].offset, 0);
        assert_eq!(result.messages[0].length, Some(1000));
        assert_eq!(result.messages[0].nominal_step, 0);
        assert_eq!(result.messages[0].step_kind, StepKind::Instant);
        assert_eq!(result.messages[0].levtype, "hag");
        assert_eq!(result.messages[0].level, Some(2));

        assert_eq!(result.messages[1].offset, 1000);
        assert_eq!(result.messages[1].length, Some(2000));
        assert_eq!(result.messages[1].nominal_step, 6);

        assert_eq!(result.messages[2].offset, 3000);
        assert_eq!(result.messages[2].length, None);
        assert_eq!(result.messages[2].nominal_step, 12);
    }

    #[test]
    fn parses_gfs_shaped_fixture() {
        let input = "\
1:0:d=2026040800:PRMSL:mean sea level:anl:
2:500:d=2026040800:TMP:2 m above ground:anl:
3:1500:d=2026040800:UGRD:10 m above ground:anl:
4:2500:d=2026040800:VGRD:10 m above ground:anl:
";
        let result = parse_wgrib2(input).expect("should parse");
        assert_eq!(result.messages.len(), 4);

        assert_eq!(result.messages[0].short_name, "PRMSL");
        assert_eq!(result.messages[0].levtype, "sfc");
        assert_eq!(result.messages[0].level, None);

        assert_eq!(result.messages[1].short_name, "TMP");
        assert_eq!(result.messages[1].levtype, "hag");
        assert_eq!(result.messages[1].level, Some(2));

        assert_eq!(result.messages[2].short_name, "UGRD");
        assert_eq!(result.messages[2].levtype, "hag");
        assert_eq!(result.messages[2].level, Some(10));

        assert_eq!(result.messages[3].short_name, "VGRD");
        assert_eq!(result.messages[3].levtype, "hag");
        assert_eq!(result.messages[3].level, Some(10));

        // Last record length stays None.
        assert_eq!(result.messages[3].length, None);
    }

    // ---------- Aggregate mixing ----------

    #[test]
    fn aggregate_records_are_dropped() {
        // APCP accumulation is dropped; instantaneous TMP is kept.
        let input = "\
1:0:d=2026040800:TMP:2 m above ground:6 hour fcst:
2:1000:d=2026040800:APCP:surface:0-6 hour acc fcst:
3:2000:d=2026040800:TMP:2 m above ground:12 hour fcst:
";
        let result = parse_wgrib2(input).expect("should parse");
        // APCP gone; TMP records survive.
        assert_eq!(result.messages.len(), 2);
        for m in &result.messages {
            assert_eq!(m.short_name, "TMP");
        }
        // Because APCP was dropped, the first TMP sees the next surviving
        // record (at offset 2000), so its length is 2000.
        assert_eq!(result.messages[0].offset, 0);
        assert_eq!(result.messages[0].length, Some(2000));
        assert_eq!(result.messages[1].offset, 2000);
        assert_eq!(result.messages[1].length, None);
    }

    #[test]
    fn max_aggregate_kept() {
        let input = "\
1:0:d=2026040800:GUST:surface:0-6 hour max fcst:
2:500:d=2026040800:GUST:surface:6-12 hour max fcst:
";
        let result = parse_wgrib2(input).expect("should parse");
        assert_eq!(result.messages.len(), 2);
        assert_eq!(
            result.messages[0].step_kind,
            StepKind::MaxOverWindow { start: 0, end: 6 }
        );
        assert_eq!(result.messages[0].nominal_step, 6);
        assert_eq!(
            result.messages[1].step_kind,
            StepKind::MaxOverWindow { start: 6, end: 12 }
        );
        assert_eq!(result.messages[1].nominal_step, 12);
    }

    // ---------- Adversarial tests ----------

    #[test]
    fn unordered_offsets_are_sorted() {
        // Records appear in the file with offsets [1000, 0, 2000]. The
        // parser must sort them and compute lengths [1000, 1000, None].
        let input = "\
2:1000:d=2026040800:TMP:2 m above ground:6 hour fcst:
1:0:d=2026040800:TMP:2 m above ground:anl:
3:2000:d=2026040800:TMP:2 m above ground:12 hour fcst:
";
        let result = parse_wgrib2(input).expect("should parse");
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages[0].offset, 0);
        assert_eq!(result.messages[0].length, Some(1000));
        assert_eq!(result.messages[1].offset, 1000);
        assert_eq!(result.messages[1].length, Some(1000));
        assert_eq!(result.messages[2].offset, 2000);
        assert_eq!(result.messages[2].length, None);
    }

    #[test]
    fn length_over_1gib_rejected() {
        // Offset 0 and 10_000_000_000 produce a length >> 1 GiB, so the
        // whole file must be rejected.
        let input = "\
1:0:d=2026040800:TMP:2 m above ground:anl:
2:10000000000:d=2026040800:TMP:2 m above ground:6 hour fcst:
";
        assert!(parse_wgrib2(input).is_none());
    }

    #[test]
    fn duplicate_offsets_are_deduped() {
        // Two records at offset 1000 — the second is a duplicate. The
        // parser keeps the first and continues.
        let input = "\
1:0:d=2026040800:TMP:2 m above ground:anl:
2:1000:d=2026040800:TMP:2 m above ground:6 hour fcst:
3:1000:d=2026040800:TMP:2 m above ground:12 hour fcst:
4:2000:d=2026040800:TMP:2 m above ground:18 hour fcst:
";
        let result = parse_wgrib2(input).expect("should parse");
        // 3 survivors: offsets 0, 1000, 2000.
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages[0].offset, 0);
        assert_eq!(result.messages[1].offset, 1000);
        assert_eq!(result.messages[2].offset, 2000);
        // The kept record at offset 1000 is the first one (6 hour fcst).
        assert_eq!(result.messages[1].nominal_step, 6);
    }

    #[test]
    fn noncontiguous_record_numbers_ok() {
        let input = "\
1:0:d=2026040800:TMP:2 m above ground:anl:
2:1000:d=2026040800:TMP:2 m above ground:6 hour fcst:
5:2000:d=2026040800:TMP:2 m above ground:12 hour fcst:
";
        let result = parse_wgrib2(input).expect("should parse");
        assert_eq!(result.messages.len(), 3);
    }

    #[test]
    fn single_record_file_ok() {
        let input = "1:0:d=2026040800:TMP:2 m above ground:anl:\n";
        let result = parse_wgrib2(input).expect("should parse");
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].length, None);
    }

    #[test]
    fn empty_file_returns_none() {
        assert!(parse_wgrib2("").is_none());
    }

    #[test]
    fn whitespace_only_file_returns_none() {
        assert!(parse_wgrib2("   \n\t\n  ").is_none());
    }

    #[test]
    fn comments_only_file_returns_none() {
        assert!(parse_wgrib2("# header\n# more\n").is_none());
    }

    #[test]
    fn malformed_line_skipped_others_survive() {
        let input = "\
1:0:d=2026040800:TMP:2 m above ground:anl:
garbage line with no colons
2:1000:d=2026040800:TMP:2 m above ground:6 hour fcst:
3:notanumber:d=2026040800:TMP:2 m above ground:12 hour fcst:
4:2000:d=2026040800:TMP:2 m above ground:18 hour fcst:
";
        let result = parse_wgrib2(input).expect("should parse");
        // Survivors: records 1, 2, 4 (at offsets 0, 1000, 2000).
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages[0].offset, 0);
        assert_eq!(result.messages[1].offset, 1000);
        assert_eq!(result.messages[2].offset, 2000);
    }

    #[test]
    fn inconsistent_reference_times_rejected() {
        let input = "\
1:0:d=2026040800:TMP:2 m above ground:anl:
2:1000:d=2026040900:TMP:2 m above ground:6 hour fcst:
";
        assert!(parse_wgrib2(input).is_none());
    }

    #[test]
    fn comments_and_blank_lines_interleaved() {
        let input = "\
# Generated by wgrib2
1:0:d=2026040800:TMP:2 m above ground:anl:

2:1000:d=2026040800:TMP:2 m above ground:6 hour fcst:
# trailing comment
";
        let result = parse_wgrib2(input).expect("should parse");
        assert_eq!(result.messages.len(), 2);
    }

    #[test]
    fn unknown_level_descriptor_skipped() {
        // The second record uses a level descriptor we do not know, so it
        // is skipped; the file still parses with one message.
        let input = "\
1:0:d=2026040800:TMP:2 m above ground:anl:
2:1000:d=2026040800:FOO:very weird level:anl:
3:2000:d=2026040800:TMP:2 m above ground:6 hour fcst:
";
        let result = parse_wgrib2(input).expect("should parse");
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[0].short_name, "TMP");
        assert_eq!(result.messages[1].short_name, "TMP");
        // Because the middle record was dropped, the first TMP spans from
        // 0 to 2000.
        assert_eq!(result.messages[0].length, Some(2000));
    }
}
