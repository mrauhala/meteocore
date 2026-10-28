//! CSV for station series (#1066): the `text/csv;header=present`
//! representation of `locations/{locationId}`, `position`, `radius` and
//! `area` results on engines that serve time series at named locations
//! (`EdrEngine::serves_station_series`), for spreadsheets, R or pandas
//! without a CoverageJSON parser.
//!
//! One layout, the long form: a header row, then one row per location, time
//! and parameter.
//!
//! ```text
//! location_id,label,longitude,latitude,datetime,parameter,value,unit
//! 101004,Helsinki Kaisaniemi,24.94,60.17,2026-09-29T00:00:00Z,air_temperature,13.4,°C
//! 101004,Helsinki Kaisaniemi,24.94,60.17,2026-09-29T00:00:00Z,wind_speed,,m/s
//! ```
//!
//! The long form needs no shared time axis, so parameters on different
//! axes and stations reporting different parameters fit as they are. Rows
//! follow the coverages of the CoverageJSON twin in order (a location list:
//! request order, the order `limit` keeps), then each series' times as it
//! has them, then its parameters by name. A missing or non-finite value is
//! a row with an empty `value`, never 0.
//!
//! - `location_id` and `label` name the location as the GeoJSON feature
//!   does (`crate::geojson`): the requested id on `/locations/{locationId}`,
//!   else the one location at the series' exact coordinates. A series no
//!   single location sits at has an empty `location_id` and the label
//!   `POINT(lon lat)`.
//! - `longitude`, `latitude` and `value` are written as the JSON writes
//!   them, the shortest text that reads back as the same number, so CSV
//!   carries the CoverageJSON's precision: a value at its range's
//!   precision (`ds_core::model::RangeValue`, #1341), a coordinate as
//!   `f64`.
//! - `datetime` is RFC 3339 in UTC with a `Z`; `unit` is the parameter's
//!   unit as CoverageJSON labels it, empty when it has none.
//!
//! Encoding: RFC 4180, UTF-8, CRLF line ends. A field holding a comma, a
//! double quote, CR or LF is enclosed in double quotes, its quotes doubled.
//!
//! Spreadsheet formula injection (OWASP "CSV Injection"): a spreadsheet
//! reads a cell starting with `=`, `+`, `-` or `@` as a formula, and also one
//! starting with a tab or CR, the two further characters OWASP lists.
//! Location ids, labels, parameter names and units come from config files,
//! databases and received reports, so text cells are guarded as OWASP
//! recommends, with a single quote `'` the cell then shows:
//!
//! - a text cell starting with one of those characters, or with `'` itself,
//!   gets a `'` in front: `-5 m` → `'-5 m`, `'t Harde` → `''t Harde`;
//! - so does such a character, `'` or `"` right after a `;` inside a text
//!   cell: Excel splits a `.csv` it opens directly at its list separator,
//!   which is `;` in many European locales, so the text after a `;` starts
//!   a cell there: `x;=A1` → `x;'=A1`;
//! - a guarded cell is always enclosed in double quotes.
//!
//! Since a `'` already at either place is itself guarded, a client reading
//! the file as data undoes the guard exactly: drop the `'` that starts a
//! text field and the `'` right after each `;` in it. Numbers and datetimes
//! are written here and are never guarded: a negative coordinate or value
//! stays a number.
//!
//! Size: a row repeats its location and is about 90 to 120 bytes, where a
//! CoverageJSON value is about 5 to 10, so a body is bounded in bytes as
//! well as by the queries' value caps: [`MAX_BYTES`]. The exact size is
//! counted first, stopping at the cap, and the body is then written into
//! one buffer of that size, never regrown.

use std::fmt;

use chrono::SecondsFormat;
use ds_core::model::{CoverageResponse, QueryResult, RangeValue, ValuePrecision};

use crate::geojson::{station_series_axes, unnamed_label, FeatureIdentity};

/// The `Content-Type` of a CSV response: [`crate::params::CSV_MEDIA_TYPE`]
/// with the charset, since RFC 4180's default is US-ASCII and labels are
/// UTF-8.
pub(crate) const CONTENT_TYPE: &str = "text/csv; charset=utf-8; header=present";

/// Most bytes one CSV body may hold, 64 MiB. The value caps (1 million per
/// position or location list, 500 000 per BUFR or PostGIS response) bound
/// the CoverageJSON body at 5 to 10 bytes a value, but a long-form row is
/// 10 to 20 times that, so 1 million rows would be about 100 MB. 64 MiB
/// still holds a 500 000-value BUFR or PostGIS response at typical labels
/// (about 110 bytes a row). Over it the query is a 400 (`QueryTooLarge`)
/// naming the ways to narrow it.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;

/// The header row, the one layout's columns.
pub const HEADER: [&str; 8] = [
    "location_id",
    "label",
    "longitude",
    "latitude",
    "datetime",
    "parameter",
    "value",
    "unit",
];

/// The characters a text cell's guard puts a `'` before at the start of the
/// cell: what makes a spreadsheet read it as a formula (OWASP "CSV
/// Injection"), and `'` itself, so that the guard can be undone exactly.
const GUARDED_AT_START: [char; 7] = ['=', '+', '-', '@', '\t', '\r', '\''];

/// The characters the guard puts a `'` before right after a `;`, where a
/// spreadsheet splitting at `;` starts a cell: those of
/// [`GUARDED_AT_START`], and `"`, which would open a quoted cell there.
const GUARDED_AFTER_SEMICOLON: [char; 8] = ['=', '+', '-', '@', '\t', '\r', '\'', '"'];

/// Why a station-series result has no CSV body.
#[derive(Debug, PartialEq, Eq)]
pub enum CsvError {
    /// A coverage that is not a station series: the kind of coverage it
    /// is. The engine broke its `serves_station_series` promise, a server
    /// fault.
    NotStationSeries(&'static str),
    /// The body would be over the byte cap it was written with.
    TooLarge,
    /// Admission refused the body's counted size: the EDR response budget
    /// is in use (#1041).
    Busy,
}

/// Encode a station-series result as CSV in the long layout (see the module
/// docs), at most `max_bytes` long ([`MAX_BYTES`] for a response).
/// `identify` names the location each coverage (by index) belongs to, as
/// for the GeoJSON encoding. Every coverage is checked before a row is
/// written, and the body's size is counted before it is allocated, then
/// admitted: `admit`'s `None` is [`CsvError::Busy`]. The response keeps
/// what `admit` returns (its budget reservation, #1041) with the body.
pub(crate) fn write_station_series<'n, A>(
    result: &CoverageResponse,
    identify: impl Fn(usize, &QueryResult) -> Option<FeatureIdentity<'n>>,
    max_bytes: usize,
    admit: impl FnOnce(usize) -> Option<A>,
) -> Result<(String, A), CsvError> {
    let coverages: &[QueryResult] = match result {
        CoverageResponse::Single(q) => std::slice::from_ref(q),
        CoverageResponse::Collection(v) => v,
    };
    for q in coverages {
        station_series_axes(q).map_err(CsvError::NotStationSeries)?;
    }
    // The same writer twice: first into a counter that stops past the cap,
    // then into one buffer of exactly the counted size.
    let mut size = ByteCount {
        bytes: 0,
        max: max_bytes,
    };
    write_rows(coverages, &identify, &mut size).map_err(|fmt::Error| CsvError::TooLarge)?;
    let admitted = admit(size.bytes).ok_or(CsvError::Busy)?;
    let mut out = String::with_capacity(size.bytes);
    // Writing into a `String` cannot fail.
    let _ = write_rows(coverages, &identify, &mut out);
    debug_assert_eq!(out.len(), size.bytes);
    Ok((out, admitted))
}

/// A `fmt::Write` sink that only counts bytes, failing once they pass `max`.
struct ByteCount {
    bytes: usize,
    max: usize,
}

impl fmt::Write for ByteCount {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.bytes = self.bytes.saturating_add(s.len());
        if self.bytes > self.max {
            return Err(fmt::Error);
        }
        Ok(())
    }
}

/// The header and every row of `coverages`, each already checked to be a
/// station series, into `out`.
fn write_rows<'n>(
    coverages: &[QueryResult],
    identify: &impl Fn(usize, &QueryResult) -> Option<FeatureIdentity<'n>>,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    for (k, column) in HEADER.iter().enumerate() {
        if k > 0 {
            out.write_char(',')?;
        }
        out.write_str(column)?;
    }
    out.write_str("\r\n")?;
    for (i, q) in coverages.iter().enumerate() {
        let Ok((x, y, times)) = station_series_axes(q) else {
            continue;
        };
        // What every row of this series repeats: location_id, label,
        // longitude, latitude.
        let mut location = String::new();
        let identity = identify(i, q);
        push_text(&mut location, identity.map_or("", |l| l.id))?;
        location.push(',');
        match identity {
            Some(identity) => push_text(&mut location, identity.label)?,
            None => push_text(&mut location, &unnamed_label(x, y))?,
        }
        location.push(',');
        push_number(&mut location, Some(x), ValuePrecision::F64)?;
        location.push(',');
        push_number(&mut location, Some(y), ValuePrecision::F64)?;
        location.push(',');

        let mut names: Vec<&String> = q.ranges.keys().collect();
        names.sort();
        // Each parameter's name and unit cells, written once per series.
        let mut parameters = Vec::with_capacity(names.len());
        let mut units = Vec::with_capacity(names.len());
        for name in &names {
            let mut cell = String::new();
            push_text(&mut cell, name)?;
            cell.push(',');
            parameters.push(cell);
            let mut cell = String::new();
            if let Some(p) = q.parameters.get(*name) {
                // CoverageJSON's `unit.label`: the unit trimmed.
                push_text(&mut cell, p.unit.trim())?;
            }
            units.push(cell);
        }
        for (step, time) in times.iter().enumerate() {
            let datetime = time.to_rfc3339_opts(SecondsFormat::AutoSi, true);
            for (k, name) in names.iter().enumerate() {
                let range = &q.ranges[*name];
                let value = range.values.get(step).copied().flatten();
                out.write_str(&location)?;
                out.write_str(&datetime)?;
                out.write_char(',')?;
                out.write_str(&parameters[k])?;
                push_number(out, value, range.precision)?;
                out.write_char(',')?;
                out.write_str(&units[k])?;
                out.write_str("\r\n")?;
            }
        }
    }
    Ok(())
}

/// A number as the JSON writes it at `precision` (`serde_json`'s shortest
/// round-trip text), or nothing for a missing or non-finite one.
fn push_number(
    out: &mut impl fmt::Write,
    value: Option<f64>,
    precision: ValuePrecision,
) -> fmt::Result {
    let Some(value) = RangeValue::new(value, precision) else {
        return Ok(());
    };
    // The longest finite `f64`, `-2.2250738585072014e-308`, is 24 bytes.
    let mut buf = [0u8; 32];
    let mut rest = &mut buf[..];
    serde_json::to_writer(&mut rest, &value).map_err(|_| fmt::Error)?;
    let len = 32 - rest.len();
    out.write_str(std::str::from_utf8(&buf[..len]).map_err(|_| fmt::Error)?)
}

/// A text cell: RFC 4180 quoting, and the formula guard (module docs).
fn push_text(out: &mut impl fmt::Write, cell: &str) -> fmt::Result {
    let guarded = cell.starts_with(GUARDED_AT_START)
        || cell
            .split(';')
            .skip(1)
            .any(|after| after.starts_with(GUARDED_AFTER_SEMICOLON));
    if !guarded && !cell.contains([',', '"', '\r', '\n']) {
        return out.write_str(cell);
    }
    out.write_char('"')?;
    // The characters guarded at this position: the cell's start, then
    // right after each `;`, where a spreadsheet splitting at `;` starts one.
    let mut guarded_here: &[char] = &GUARDED_AT_START;
    for c in cell.chars() {
        if guarded_here.contains(&c) {
            out.write_char('\'')?;
        }
        if c == '"' {
            out.write_char('"')?;
        }
        out.write_char(c)?;
        guarded_here = if c == ';' {
            &GUARDED_AFTER_SEMICOLON
        } else {
            &[]
        };
    }
    out.write_char('"')
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{DateTime, Utc};
    use ds_core::model::{DomainDescription, NdArray, ParameterDescription};

    use super::*;

    /// The body alone, admitted whatever its size.
    fn unadmitted<'n>(
        result: &CoverageResponse,
        identify: impl Fn(usize, &QueryResult) -> Option<FeatureIdentity<'n>>,
        max_bytes: usize,
    ) -> Result<String, CsvError> {
        write_station_series(result, identify, max_bytes, |_| Some(())).map(|(body, ())| body)
    }

    fn series(
        x: f64,
        y: f64,
        hours: &[u32],
        values: &[(&str, &str, Vec<Option<f64>>)],
    ) -> QueryResult {
        let t: Vec<DateTime<Utc>> = hours
            .iter()
            .map(|h| format!("2026-01-01T{h:02}:00:00Z").parse().unwrap())
            .collect();
        let mut parameters = HashMap::new();
        let mut ranges = HashMap::new();
        for (name, unit, v) in values {
            parameters.insert(
                name.to_string(),
                ParameterDescription {
                    label: name.to_string(),
                    unit: unit.to_string(),
                    observed_property: name.to_string(),
                    standard_name: None,
                },
            );
            ranges.insert(
                name.to_string(),
                NdArray {
                    shape: vec![v.len()],
                    axis_names: vec!["t".into()],
                    values: v.clone(),
                    precision: ValuePrecision::F64,
                },
            );
        }
        QueryResult {
            domain: DomainDescription::PointSeries { x, y, t, z: None },
            parameters,
            ranges,
        }
    }

    fn named<'a>(id: &'a str, label: &'a str) -> Option<FeatureIdentity<'a>> {
        Some(FeatureIdentity { id, label })
    }

    fn text(cell: &str) -> String {
        let mut out = String::new();
        push_text(&mut out, cell).unwrap();
        out
    }

    /// The documented way to undo the guard on a field already read as RFC
    /// 4180: drop the `'` that starts it and the `'` right after each `;`.
    fn unguard(field: &str) -> String {
        let mut out = String::new();
        let mut cell_start = true;
        for c in field.chars() {
            if !(cell_start && c == '\'') {
                out.push(c);
            }
            cell_start = c == ';';
        }
        out
    }

    /// An RFC 4180 field's content: the quotes around it removed and its
    /// doubled quotes undone.
    fn unquote(written: &str) -> String {
        match written.strip_prefix('"') {
            Some(inner) => inner.strip_suffix('"').unwrap().replace("\"\"", "\""),
            None => written.to_string(),
        }
    }

    #[test]
    fn long_form_rows_per_time_and_parameter() {
        let result = CoverageResponse::Single(series(
            24.26,
            -63.0,
            &[0, 1],
            &[
                ("wind_speed", "m/s", vec![Some(3.5), None]),
                ("air_temperature", "°C", vec![Some(-17.9), Some(f64::NAN)]),
            ],
        ));
        let csv = unadmitted(&result, |_, _| named("Alajärvi Möksy", "Möksy"), MAX_BYTES).unwrap();
        assert_eq!(
            csv,
            "location_id,label,longitude,latitude,datetime,parameter,value,unit\r\n\
             Alajärvi Möksy,Möksy,24.26,-63.0,2026-01-01T00:00:00Z,air_temperature,-17.9,°C\r\n\
             Alajärvi Möksy,Möksy,24.26,-63.0,2026-01-01T00:00:00Z,wind_speed,3.5,m/s\r\n\
             Alajärvi Möksy,Möksy,24.26,-63.0,2026-01-01T01:00:00Z,air_temperature,,°C\r\n\
             Alajärvi Möksy,Möksy,24.26,-63.0,2026-01-01T01:00:00Z,wind_speed,,m/s\r\n"
        );
        // The counted size is the body's, and the buffer was never regrown.
        assert_eq!(csv.capacity(), csv.len());
    }

    #[test]
    fn values_keep_the_json_text() {
        let values = [0.1, 1.0, -0.0, 1e-7, 1e21, 123_456.789_012_345_67];
        let result = CoverageResponse::Single(series(
            0.0,
            0.0,
            &[0, 1, 2, 3, 4, 5],
            &[("v", "", values.iter().map(|v| Some(*v)).collect())],
        ));
        let csv = unadmitted(&result, |_, _| None, MAX_BYTES).unwrap();
        let written: Vec<&str> = csv
            .lines()
            .skip(1)
            .map(|row| row.split(',').nth(6).unwrap())
            .collect();
        let json: Vec<String> = values
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect();
        assert_eq!(written, json);
        // An unnamed series: no id, its point as the label; no unit.
        assert!(csv
            .lines()
            .nth(1)
            .unwrap()
            .starts_with(",POINT(0 0),0.0,0.0,"));
        assert!(csv.lines().nth(1).unwrap().ends_with(",0.1,"));
    }

    /// A value cell carries its range's precision as the CoverageJSON
    /// does (#1341): an `f32` range's values as their shortest `f32` text.
    /// Coordinates stay `f64`.
    #[test]
    fn values_keep_the_range_precision() {
        let values = [
            f64::from(10.531_f32),
            f64::from(283.681_f32) - 273.15,
            f64::from(0.1_f32),
            1e300,
        ];
        let mut q = series(
            24.1,
            60.2,
            &[0, 1, 2, 3],
            &[("t2m", "°C", values.iter().map(|v| Some(*v)).collect())],
        );
        q.ranges.get_mut("t2m").unwrap().precision = ValuePrecision::F32;
        let json = serde_json::to_string(&crate::response::Values(&q.ranges["t2m"])).unwrap();
        let csv = unadmitted(&CoverageResponse::Single(q), |_, _| None, MAX_BYTES).unwrap();
        let written: Vec<&str> = csv
            .lines()
            .skip(1)
            .map(|row| row.split(',').nth(6).unwrap())
            .collect();
        assert_eq!(written, ["10.531", "10.531", "0.1", "1e+300"]);
        assert_eq!(format!("[{}]", written.join(",")), json);
        assert!(csv
            .lines()
            .nth(1)
            .unwrap()
            .starts_with(",POINT(24.1 60.2),24.1,60.2,"));
    }

    /// The byte cap: a body of exactly `max_bytes` is written, one byte
    /// more is refused before anything is allocated for it, and a cap far
    /// below the body stops the count early.
    #[test]
    fn bodies_over_the_byte_cap_are_refused() {
        let coverages: Vec<QueryResult> = (0..50)
            .map(|i| {
                series(
                    f64::from(i),
                    60.0,
                    &[0, 1, 2],
                    &[("t", "K", vec![Some(273.15), None, Some(-1.5)])],
                )
            })
            .collect();
        let result = CoverageResponse::Collection(coverages);
        let body = unadmitted(&result, |_, _| named("id", "Station"), MAX_BYTES).unwrap();
        assert_eq!(body.lines().count(), 1 + 50 * 3);
        assert_eq!(
            unadmitted(&result, |_, _| named("id", "Station"), body.len()).unwrap(),
            body
        );
        assert_eq!(
            unadmitted(&result, |_, _| named("id", "Station"), body.len() - 1),
            Err(CsvError::TooLarge)
        );
        assert_eq!(
            unadmitted(&result, |_, _| named("id", "Station"), 10),
            Err(CsvError::TooLarge)
        );
        // The README and the `f` description in `/api` state it as 64 MiB.
        assert_eq!(MAX_BYTES, 64 << 20);
        // Admission (#1041) is asked for the exact size before the body is
        // allocated, and its refusal is `Busy`.
        let mut asked = None;
        let admitted = write_station_series(
            &result,
            |_, _| named("id", "Station"),
            MAX_BYTES,
            |size| asked.replace(size).map_or(Some(()), |_| None),
        );
        assert_eq!(admitted, Ok((body.clone(), ())));
        assert_eq!(asked, Some(body.len()));
        assert_eq!(
            write_station_series(&result, |_, _| None, MAX_BYTES, |_| None::<()>),
            Err(CsvError::Busy)
        );
    }

    #[test]
    fn text_cells_are_quoted_per_rfc_4180() {
        assert_eq!(text(r#"Pier 4, "North" end"#), r#""Pier 4, ""North"" end""#);
        for (cell, written) in [
            ("plain", "plain"),
            ("two\r\nlines", "\"two\r\nlines\""),
            ("O'Hare", "O'Hare"),
            ("a;b", "a;b"),
            ("", ""),
        ] {
            assert_eq!(text(cell), written, "{cell:?}");
        }
    }

    #[test]
    fn formula_cells_are_prefixed_and_quoted() {
        for (cell, written) in [
            ("=HYPERLINK(\"x\")", "\"'=HYPERLINK(\"\"x\"\")\""),
            ("+1", "\"'+1\""),
            ("-5 m", "\"'-5 m\""),
            ("@SUM(A1)", "\"'@SUM(A1)\""),
            ("\tcmd", "\"'\tcmd\""),
            ("\rcmd", "\"'\rcmd\""),
            ("a=b", "a=b"),
            // A genuine leading `'` is guarded too, so that dropping the
            // one the guard adds gives the label back.
            ("'t Harde, NL", "\"''t Harde, NL\""),
            ("'s-Hertogenbosch", "\"''s-Hertogenbosch\""),
            // A `;` starts a cell where it is the list separator.
            ("x;=cmd|' /C calc'!A0;y", "\"x;'=cmd|' /C calc'!A0;y\""),
            ("a;;-1", "\"a;;'-1\""),
            ("a;\"=1\"", "\"a;'\"\"=1\"\"\""),
            ("a;'b", "\"a;''b\""),
            ("a; =b", "a; =b"),
            ("=a;@b", "\"'=a;'@b\""),
        ] {
            assert_eq!(text(cell), written, "{cell:?}");
        }
    }

    /// The documented undo, dropping the `'` that starts a field and the
    /// `'` right after each `;`, gives every text cell back exactly.
    #[test]
    fn the_guard_is_undone_exactly() {
        for cell in [
            "plain",
            "O'Hare",
            "'t Harde, NL",
            "'s-Hertogenbosch \"centrum\"",
            "''",
            "'",
            "-5 m",
            "=HYPERLINK(\"x\")",
            "x;=cmd;'y;\"z\";;@w",
            "a;'b",
            "a;",
            ";",
            ";'",
            "",
        ] {
            assert_eq!(unguard(&unquote(&text(cell))), cell, "{cell:?}");
        }
    }

    #[test]
    fn non_series_coverages_are_refused_before_any_row() {
        let grid = QueryResult {
            domain: DomainDescription::Grid {
                x: vec![1.0],
                y: vec![2.0],
                t: None,
                z: None,
            },
            parameters: HashMap::new(),
            ranges: HashMap::new(),
        };
        let result = CoverageResponse::Collection(vec![
            series(1.0, 2.0, &[0], &[("v", "", vec![Some(1.0)])]),
            grid,
        ]);
        assert_eq!(
            unadmitted(&result, |_, _| None, MAX_BYTES),
            Err(CsvError::NotStationSeries("Grid"))
        );
        // Checked before the size: a cap the body cannot meet still names
        // the broken coverage.
        assert_eq!(
            unadmitted(&result, |_, _| None, 0),
            Err(CsvError::NotStationSeries("Grid"))
        );
        let empty = unadmitted(
            &CoverageResponse::Collection(Vec::new()),
            |_, _| None,
            MAX_BYTES,
        );
        assert_eq!(empty.unwrap(), format!("{}\r\n", HEADER.join(",")));
    }
}
