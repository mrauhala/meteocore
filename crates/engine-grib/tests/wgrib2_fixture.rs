//! Integration test that parses a real NOAA GFS wgrib2 index file.
//!
//! The fixture was downloaded from
//! `s3://noaa-gfs-bdp-pds/gfs.YYYYMMDD/00/atmos/gfs.t00z.pgrb2.0p25.f006.idx`
//! and committed under `testdata/gfs/`.

use engine_grib::wgrib2_index::{parse_wgrib2, StepKind};

fn load_fixture() -> Option<String> {
    let path = std::path::Path::new("../../testdata/gfs/gfs.t00z.pgrb2.0p25.f006.idx");
    std::fs::read_to_string(path).ok()
}

#[test]
fn parses_real_gfs_fixture() {
    let Some(content) = load_fixture() else {
        eprintln!("Skipping: GFS fixture not present");
        return;
    };

    let parsed = parse_wgrib2(&content).expect("real GFS idx should parse");

    // Reference time is derived from the fixture filename prefix (gfs.YYYYMMDD/00).
    // We don't hardcode the date — just check that the year/month is plausible
    // and the hour is one of the GFS runs.
    let rt = parsed.reference_time;
    assert!(rt.timestamp() > 0, "reference time must be valid");
    assert!(
        rt.timestamp() > 1_700_000_000,
        "reference time must be reasonably recent"
    );
    let hour = rt.format("%H").to_string();
    assert!(
        ["00", "06", "12", "18"].contains(&hour.as_str()),
        "GFS run hour {hour} should be one of 00/06/12/18"
    );

    // A f006 pgrb2.0p25 file has ~700 distinct messages. After aggregate
    // filtering (dropping acc/ave records), we still expect hundreds.
    assert!(
        parsed.messages.len() > 300,
        "expected at least 300 instantaneous messages, got {}",
        parsed.messages.len()
    );

    // At least one well-known near-surface field must be present.
    assert!(
        parsed
            .messages
            .iter()
            .any(|m| m.short_name == "PRMSL" && m.levtype == "sfc"),
        "PRMSL at mean sea level must be present"
    );

    // At least one 2 m temperature at hag=2.
    assert!(
        parsed
            .messages
            .iter()
            .any(|m| m.short_name == "TMP" && m.levtype == "hag" && m.level == Some(2)),
        "TMP at 2 m above ground must be present"
    );

    // At least one pressure-level field.
    assert!(
        parsed
            .messages
            .iter()
            .any(|m| m.levtype == "pl" && m.level.is_some()),
        "at least one pressure-level message must survive"
    );

    // Aggregates (acc/ave) must have been dropped; no message should carry a
    // StepKind we haven't seen in the real file. GUST in f006 is typically
    // a 0-6 hour max fcst — with our current plan we keep max/min aggregates
    // coerced to the window end.
    for m in &parsed.messages {
        match m.step_kind {
            StepKind::Instant => {}
            StepKind::MaxOverWindow { .. } => {}
            StepKind::MinOverWindow { .. } => {}
        }
    }

    // Lengths are consistent: all but the last record must have a concrete
    // length, and all concrete lengths are strictly positive.
    let mut unset_count = 0;
    for m in &parsed.messages {
        match m.length {
            Some(l) => assert!(l > 0, "length must be positive"),
            None => unset_count += 1,
        }
    }
    assert_eq!(
        unset_count, 1,
        "exactly one record (the last) should have an unresolved length"
    );
    // The unresolved record must be the one with the largest offset.
    let max_offset_idx = parsed
        .messages
        .iter()
        .enumerate()
        .max_by_key(|(_, m)| m.offset)
        .map(|(i, _)| i)
        .unwrap();
    assert!(parsed.messages[max_offset_idx].length.is_none());
}
