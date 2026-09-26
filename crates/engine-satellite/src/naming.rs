//! GOES-R ABI file naming on the NOAA open-data buckets:
//! `<product>/<year>/<day of year>/<hour>/OR_<product>-M<mode>[C<band>]_G<satellite>_s<start>_e<end>_c<created>.nc`,
//! where each time stamp is `%Y%j%H%M%S` plus a tenths-of-a-second digit.

use chrono::{DateTime, NaiveDateTime, Utc};
use regex::Regex;

/// Where one product's files live and how their names encode the scan time.
pub(crate) struct Naming {
    /// Hourly strftime prefix listing the product's files, e.g.
    /// `ABI-L2-CMIPF/%Y/%j/%H/`.
    pub prefix: String,
    file: Regex,
}

impl Naming {
    /// The GOES-R layout of `product` (e.g. `ABI-L2-CMIPF`), restricted to
    /// one ABI `band` for per-band products.
    pub fn goes_r(product: &str, band: Option<u8>) -> Self {
        let band = band.map(|b| format!("C{b:02}")).unwrap_or_default();
        let file = Regex::new(&format!(
            r"^OR_{}-M\d+{band}_G\d+_s(\d{{13}})\d_e\d{{14}}_c\d{{14}}\.nc$",
            regex::escape(product)
        ))
        .expect("config validation restricts the product to [A-Za-z0-9-]");
        Naming {
            prefix: format!("{product}/%Y/%j/%H/"),
            file,
        }
    }

    /// The scan start of `basename`, to the minute, when it is one of this
    /// product's files.
    ///
    /// A full-disk scan starts about 20 s past its nominal ten-minute slot
    /// (`s20262681900199` is 19:00:19.9). Keying frames on the minute lets
    /// a request for the nominal 19:00 find that scan rather than snap back
    /// to the one before it; every ABI scan mode starts at most one scan per
    /// minute, so the minute still identifies the scan.
    pub fn scan_start(&self, basename: &str) -> Option<DateTime<Utc>> {
        let digits = self.file.captures(basename)?.get(1)?.as_str();
        NaiveDateTime::parse_from_str(&format!("{}00", &digits[..11]), "%Y%j%H%M%S")
            .ok()
            .map(|t| t.and_utc())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goes_r_names_select_product_band_and_scan_minute() {
        let c13 = Naming::goes_r("ABI-L2-CMIPF", Some(13));
        assert_eq!(c13.prefix, "ABI-L2-CMIPF/%Y/%j/%H/");
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
}
