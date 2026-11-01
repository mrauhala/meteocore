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

    // A f006 pgrb2.0p25 file has hundreds of supported messages.
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
            .any(|m| m.short_name == "PRMSL" && m.levtype == "msl"),
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

    assert!(parsed.messages.iter().any(|m| m.short_name == "APCP"
        && matches!(m.step_kind, StepKind::Accumulation { start: 0, end: 6 })));
    assert!(parsed
        .messages
        .iter()
        .any(|m| matches!(m.step_kind, StepKind::Average { .. })));

    // Every message ends where the next record of the index starts, the
    // records the parser skips included (fractional-mb levels, PV
    // surfaces): measured to the next record kept, GUST:surface spanned the
    // 0.01-0.7 mb levels after it, 68 MB instead of 0.6 MB. Only a message
    // at the index's last offset has no length.
    let mut starts: Vec<u64> = content
        .lines()
        .filter_map(|line| line.split(':').nth(1)?.parse().ok())
        .collect();
    starts.sort_unstable();
    for m in &parsed.messages {
        let next = starts.iter().copied().find(|&start| start > m.offset);
        assert_eq!(
            m.length,
            next.map(|next| next - m.offset),
            "{} {:?} {:?} at offset {}",
            m.short_name,
            m.levtype,
            m.level,
            m.offset
        );
    }
    let gust = parsed
        .messages
        .iter()
        .find(|m| m.short_name == "GUST" && m.levtype == "sfc")
        .expect("GUST at the surface must be present");
    assert!(
        gust.length.is_some_and(|l| l < 2_000_000),
        "GUST:surface is about 0.6 MB, got {:?}",
        gust.length
    );
}
