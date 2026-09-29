//! File naming on the providers' open-data buckets.
//!
//! - GOES-R ABI (`goes-r`):
//!   `<product>/<year>/<day of year>/<hour>/OR_<product>-M<mode>[C<band>]_G<satellite>_s<start>_e<end>_c<created>.nc`,
//!   one file per scan, each time stamp `%Y%j%H%M%S` plus a tenths digit.
//! - Himawari ISatSS (`isatss`), the AWIPS tiles NOAA republishes for
//!   Himawari-9:
//!   `AHI-L2-FLDK-ISatSS/<year>/<month>/<day>/<hour><minute>/OR_<sector>-<res>-B<bits>-M<mode>C<band>-T<tile>_G<satellite>_s<start>_c<created>.nc`,
//!   one file per tile (88 for a full disk), `<start>` `%Y%j%H%M%S` plus a
//!   tenths digit.
//! - GK2A AMI L1B (`gk2a`), as NOAA republishes KMA's files:
//!   `AMI/L1B/<sector>/<year><month>/<day>/<hour>/gk2a_ami_le1b_<channel>_<sector><res>ge_<slot>.nc`,
//!   one file per channel and scan, `<slot>` the nominal `%Y%m%d%H%M` (the
//!   scan itself starts ~30 s later).

use chrono::{DateTime, Duration, DurationRound, NaiveDateTime, Utc};
use ds_core::error::DataServerError;
use ds_storage::discovery::{expand_prefix_for_range, validate_prefix_pattern, TimeWindow};
use regex::Regex;

/// Full-disk cadence of the ISatSS scan directories: one `%H%M/` per
/// ten-minute slot, the other minutes holding regional sectors.
const ISATSS_SLOT_MINUTES: i64 = 10;

/// Longest window over ISatSS slot directories: bootstrapping lists every
/// slot of it once (36 sequential lists), later polls only the slots not
/// yet ingested.
const MAX_ISATSS_WINDOW_HOURS: i64 = 6;

/// How a provider partitions its bucket.
enum Layout {
    /// Hourly strftime prefix listing the product's files, e.g.
    /// `ABI-L2-CMIPF/%Y/%j/%H/`.
    Hourly(String),
    /// One directory per ten-minute scan slot, holding every band's tiles.
    IsatssSlots,
}

/// Where one product's files live and how their names encode the scan time
/// (and, for a tiled product, the tile).
pub(crate) struct Naming {
    layout: Layout,
    /// `start` captures the scan start stamp; `tile`, when present, the
    /// tile number.
    file: Regex,
    /// How the first digits of `start` read, to the minute.
    stamp: Stamp,
}

/// The minute-precision prefix of a file's scan start stamp.
#[derive(Clone, Copy)]
enum Stamp {
    /// `%Y%j%H%M` (GOES-R, ISatSS: day of year).
    DayOfYear,
    /// `%Y%m%d%H%M` (GK2A).
    Calendar,
}

impl Stamp {
    /// The digits read and their format.
    fn format(self) -> (usize, &'static str) {
        match self {
            Stamp::DayOfYear => (11, "%Y%j%H%M"),
            Stamp::Calendar => (12, "%Y%m%d%H%M"),
        }
    }
}

impl Naming {
    /// The GOES-R layout of `product` (e.g. `ABI-L2-CMIPF`), restricted to
    /// one ABI `band` for per-band products.
    pub fn goes_r(product: &str, band: Option<u8>) -> Self {
        let band = band.map(|b| format!("C{b:02}")).unwrap_or_default();
        let file = Regex::new(&format!(
            r"^OR_{}-M\d+{band}_G\d+_s(?P<start>\d{{13}})\d_e\d{{14}}_c\d{{14}}\.nc$",
            regex::escape(product)
        ))
        .expect("config validation restricts the product to [A-Za-z0-9-]");
        Naming {
            layout: Layout::Hourly(format!("{product}/%Y/%j/%H/")),
            file,
            stamp: Stamp::DayOfYear,
        }
    }

    /// The Himawari ISatSS tiles of `sector` (`HFD`, the full disk) in one
    /// AHI `band`.
    pub fn isatss(sector: &str, band: u8) -> Self {
        let file = Regex::new(&format!(
            r"^OR_{}-\d{{3}}-B\d{{2}}-M\d+C{band:02}-T(?P<tile>\d{{3}})_G[A-Z0-9]+_s(?P<start>\d{{13}})\d_c\d{{14}}\.nc$",
            regex::escape(sector)
        ))
        .expect("config validation restricts the product to [A-Za-z0-9-]");
        Naming {
            layout: Layout::IsatssSlots,
            file,
            stamp: Stamp::DayOfYear,
        }
    }

    /// The GK2A AMI L1B files of one `sector` (`FD`, the full disk) and
    /// `channel` (`ir105`) at its resolution code (`020`, 2 km).
    pub fn gk2a(sector: &str, channel: &str, resolution: &str) -> Self {
        let file = Regex::new(&format!(
            r"^gk2a_ami_le1b_{}_{}{}ge_(?P<start>\d{{12}})\.nc$",
            regex::escape(channel),
            regex::escape(&sector.to_ascii_lowercase()),
            regex::escape(resolution),
        ))
        .expect("the channel table and config validation restrict the parts");
        Naming {
            layout: Layout::Hourly(format!("AMI/L1B/{sector}/%Y%m/%d/%H/")),
            file,
            stamp: Stamp::Calendar,
        }
    }

    /// Checks a bucket source's `window`: hourly prefixes are capped at
    /// 24 h (`validate_prefix_pattern`), slot directories at
    /// [`MAX_ISATSS_WINDOW_HOURS`].
    pub fn validate_window(&self, window: Option<&TimeWindow>) -> Result<(), DataServerError> {
        match &self.layout {
            Layout::Hourly(pattern) => validate_prefix_pattern(pattern, window).map(|_| ()),
            Layout::IsatssSlots => {
                let (start, end) = window
                    .ok_or_else(|| {
                        DataServerError::Config("an ISatSS bucket needs a time_window".into())
                    })?
                    .to_range(Utc::now());
                if end - start > Duration::hours(MAX_ISATSS_WINDOW_HOURS) {
                    return Err(DataServerError::Config(format!(
                        "an ISatSS bucket lists one directory per ten-minute scan; its \
                         time_window may span at most {MAX_ISATSS_WINDOW_HOURS} hours"
                    )));
                }
                Ok(())
            }
        }
    }

    /// Whether one scan is many files, one per tile.
    pub fn tiled(&self) -> bool {
        matches!(self.layout, Layout::IsatssSlots)
    }

    /// The listing prefixes holding scans that start in `start..=end`,
    /// skipping the scan slots `known` says are already ingested (a slot
    /// directory holds one scan, so there is nothing new to find there).
    pub fn prefixes(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        known: impl Fn(DateTime<Utc>) -> bool,
    ) -> Result<Vec<String>, DataServerError> {
        match &self.layout {
            Layout::Hourly(pattern) => expand_prefix_for_range(pattern, start, end),
            Layout::IsatssSlots => {
                let step = Duration::minutes(ISATSS_SLOT_MINUTES);
                let mut slot = start
                    .duration_trunc(step)
                    .map_err(|e| DataServerError::Config(format!("scan slot: {e}")))?;
                let mut prefixes = Vec::new();
                while slot <= end {
                    if !known(slot) {
                        prefixes.push(slot.format("AHI-L2-FLDK-ISatSS/%Y/%m/%d/%H%M").to_string());
                    }
                    slot += step;
                }
                Ok(prefixes)
            }
        }
    }

    /// The scan start of `basename`, to the minute, when it is one of this
    /// product's files.
    ///
    /// A GOES full-disk scan starts about 20 s past its nominal ten-minute
    /// slot (`s20262681900199` is 19:00:19.9). Keying frames on the minute
    /// lets a request for the nominal 19:00 find that scan rather than snap
    /// back to the one before it; every ABI scan mode starts at most one
    /// scan per minute, so the minute still identifies the scan.
    pub fn scan_start(&self, basename: &str) -> Option<DateTime<Utc>> {
        let digits = self.file.captures(basename)?.name("start")?.as_str();
        let (len, format) = self.stamp.format();
        NaiveDateTime::parse_from_str(&format!("{}00", digits.get(..len)?), &format!("{format}%S"))
            .ok()
            .map(|t| t.and_utc())
    }

    /// The tile number of `basename`, for a tiled product's file.
    pub fn tile(&self, basename: &str) -> Option<u32> {
        self.file
            .captures(basename)?
            .name("tile")?
            .as_str()
            .parse()
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goes_r_names_select_product_band_and_scan_minute() {
        let c13 = Naming::goes_r("ABI-L2-CMIPF", Some(13));
        assert!(!c13.tiled());
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            c13.prefixes(
                at("2026-09-25T18:30:00Z"),
                at("2026-09-25T19:10:00Z"),
                |_| true
            )
            .unwrap(),
            ["ABI-L2-CMIPF/2026/268/18", "ABI-L2-CMIPF/2026/268/19"]
        );
        let name = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
        assert_eq!(
            c13.scan_start(name),
            Some("2026-09-25T19:00:00Z".parse().unwrap())
        );
        // Other bands, other products, partial uploads and suffixes are not it.
        for other in [
            "OR_ABI-L2-CMIPF-M6C02_G19_s20262681900199_e20262681909507_c20262681909569.nc",
            "OR_ABI-L2-ACHTF-M6_G19_s20262681900199_e20262681909507_c20262681912337.nc",
            "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc.tmp",
            "OR_ABI-L2-CMIPF-M6C13_G19_s2026268190019_e20262681909519_c20262681909592.nc",
        ] {
            assert_eq!(c13.scan_start(other), None, "{other}");
        }
        let acht = Naming::goes_r("ABI-L2-ACHTF", None);
        assert_eq!(
            acht.scan_start(
                "OR_ABI-L2-ACHTF-M6_G19_s20262681910199_e20262681919507_c20262681922250.nc"
            ),
            Some("2026-09-25T19:10:00Z".parse().unwrap())
        );
    }

    #[test]
    fn gk2a_names_select_sector_channel_and_slot() {
        let ir105 = Naming::gk2a("FD", "ir105", "020");
        assert!(!ir105.tiled());
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            ir105
                .prefixes(
                    at("2026-09-28T23:30:00Z"),
                    at("2026-09-29T00:10:00Z"),
                    |_| false
                )
                .unwrap(),
            ["AMI/L1B/FD/202609/28/23", "AMI/L1B/FD/202609/29/00"]
        );
        let name = "gk2a_ami_le1b_ir105_fd020ge_202609281200.nc";
        assert_eq!(ir105.scan_start(name), Some(at("2026-09-28T12:00:00Z")));
        for other in [
            // Another channel, resolution, sector; a partial upload; a
            // malformed stamp.
            "gk2a_ami_le1b_ir112_fd020ge_202609281200.nc",
            "gk2a_ami_le1b_ir105_fd010ge_202609281200.nc",
            "gk2a_ami_le1b_ir105_la020ge_202609281200.nc",
            "gk2a_ami_le1b_ir105_fd020ge_202609281200.nc.tmp",
            "gk2a_ami_le1b_ir105_fd020ge_20260928120.nc",
            "gk2a_ami_le1b_ir105_fd020ge_202613281200.nc",
        ] {
            assert_eq!(ir105.scan_start(other), None, "{other}");
        }
    }

    #[test]
    fn isatss_names_select_sector_band_tile_and_scan_minute() {
        let c13 = Naming::isatss("HFD", 13);
        assert!(c13.tiled());
        let name = "OR_HFD-020-B12-M1C13-T007_GH9_s20262701920000_c20262701928130.nc";
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(c13.scan_start(name), Some(at("2026-09-27T19:20:00Z")));
        assert_eq!(c13.tile(name), Some(7));
        for other in [
            // Another band, a regional sector, another resolution's band.
            "OR_HFD-020-B12-M1C14-T007_GH9_s20262701920000_c20262701928130.nc",
            "OR_HR3-020-B12-M1C13-T001_GH9_s20262701929000_c20262701931000.nc",
            "OR_HFD-005-B11-M1C03-T001_GH9_s20262670000000_c20262670008450.nc",
            "OR_HFD-020-B12-M1C13-T007_GH9_s20262701920000_c20262701928130.nc.tmp",
        ] {
            assert_eq!(c13.scan_start(other), None, "{other}");
        }
        // One directory per ten-minute slot, skipping slots already held.
        let held = at("2026-09-27T19:10:00Z");
        assert_eq!(
            c13.prefixes(
                at("2026-09-27T18:55:00Z"),
                at("2026-09-27T19:20:00Z"),
                |t| t == held
            )
            .unwrap(),
            [
                "AHI-L2-FLDK-ISatSS/2026/09/27/1850",
                "AHI-L2-FLDK-ISatSS/2026/09/27/1900",
                "AHI-L2-FLDK-ISatSS/2026/09/27/1920",
            ]
        );
    }
}
