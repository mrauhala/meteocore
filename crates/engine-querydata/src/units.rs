//! The unit of each FMI newbase parameter number (#1211).
//!
//! A `.sqd` parameter descriptor carries a number, a name and a value
//! range, but no unit string: the number fixes the unit by FMI convention,
//! the role a WMO code plays for GRIB. newbase itself keeps no unit table
//! (`newbase/NFmiParameterName.h` lists names only), so each unit below is
//! the one FMI's own code states for that number, at
//! fmidev/smartmet-library-newbase 3caf1b375d4e95c88bbd6d0a68c1c317de528c83
//! and fmidev/smartmet-qdtools a25f6fea4d93283aaecfbda8bc14247396f042a5:
//!
//! - newbase `newbase/NFmiTotalWind.h` `FmiWindStruct`: `WindSpeedMS` and
//!   `WindGustMS` in m/s, direction in 10° steps; `NFmiTotalWind.cpp`
//!   `CreateParam`: `kFmiWindDirection` over 0–360, `kFmiHourlyMaximumGust`
//!   named `MaxGustMS`, `kFmiWindUMS`/`kFmiWindVMS` the m/s speed's
//!   components.
//! - qdtools `source/NcFileExtended.cpp` `apply_unit_conversion`: nctoqd
//!   stores K as °C, Pa as hPa and a fraction as percent.
//! - qdtools `cnf/grib.conf`, gribtoqd's table: temperature and dew point
//!   offset by −273.15 to °C, `Pressure` scaled by 0.01 to hPa, relative
//!   humidity and cloud covers stored as GRIB's %, u/v and gust as GRIB's
//!   m/s, `kg m**-2` precipitation unscaled into `PrecipitationAmount` (mm),
//!   and `prate` scaled by 10 800 s into `Precipitation3h`, the 3 h amount
//!   in mm. newbase's `NFmiQueryDataUtil.cpp` `ReadPrecipitation` divides
//!   `Precipitation3h`/`6h` by their hours to stand in for
//!   `Precipitation1h`: the same mm.
//!
//! The values are stored in these units, so they are served as stored,
//! without conversion. A number not listed reports no unit, never a guess:
//! `GeopHeight` (2), for one, also receives geopotential in m² s⁻² unscaled
//! from gribtoqd (`129;2`), so its stored unit is not fixed. The spellings
//! are the display forms engine-grib serves.
//!
//! `MaximumTemperature` (5) and `MinimumTemperature` (6) are left out on
//! purpose: gribtoqd offsets ECMWF's into °C but copies DWD's Kelvin in
//! unconverted (`grib.conf` rows `121;5` and `122;6`, base 0, scale 1).
//! So is `PrecipitationRate` (49), written in two units 3600× apart:
//! newbase's `NFmiQueryDataUtil.cpp` fills it as `"exact rr1h"` and h5toqd
//! maps ODIM `RATE` into it, both mm/h, but bufrtoqd copies BUFR 013055
//! (`cnf/bufr.conf`, kg m⁻² s⁻¹) in unconverted (`normal_value`), as
//! nctoqd does CF `precipitation_flux` in `docs/nctoqd.md`'s sample table.
const UNITS: &[(u32, &str)] = &[
    (1, "hPa"),     // kFmiPressure
    (4, "°C"),      // kFmiTemperature
    (10, "°C"),     // kFmiDewPoint
    (13, "%"),      // kFmiHumidity (relative)
    (20, "°"),      // kFmiWindDirection
    (21, "m s-1"),  // kFmiWindSpeedMS
    (23, "m s-1"),  // kFmiWindUMS
    (24, "m s-1"),  // kFmiWindVMS
    (50, "mm"),     // kFmiPrecipitationAmount
    (79, "%"),      // kFmiTotalCloudCover
    (273, "%"),     // kFmiLowCloudCover
    (274, "%"),     // kFmiMediumCloudCover
    (275, "%"),     // kFmiHighCloudCover
    (353, "mm"),    // kFmiPrecipitation1h
    (354, "mm"),    // kFmiPrecipitation3h, a 3 h amount
    (417, "m s-1"), // kFmiWindGust
    (467, "m s-1"), // kFmiHourlyMaximumGust
];

/// The unit of FMI newbase parameter `number`'s values, `""` when the
/// number has no sourced unit.
pub(crate) fn unit(number: u32) -> &'static str {
    UNITS
        .binary_search_by_key(&number, |&(n, _)| n)
        .map_or("", |i| UNITS[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_without_duplicates() {
        assert!(UNITS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn numbers_report_their_newbase_units() {
        for (number, want) in [
            (1, "hPa"),
            (4, "°C"),
            (10, "°C"),
            (13, "%"),
            (20, "°"),
            (21, "m s-1"),
            (79, "%"),
            (353, "mm"),
            (417, "m s-1"),
        ] {
            assert_eq!(unit(number), want, "{number}");
        }
    }

    /// A number without a sourced unit reports none: unknown to newbase,
    /// or known with no fixed unit, as the maximum and minimum temperatures
    /// gribtoqd fills with °C or Kelvin and the precipitation rate the
    /// converters fill with mm/h or kg m⁻² s⁻¹.
    #[test]
    fn an_unsourced_number_reports_no_unit() {
        for number in [0, 2, 5, 6, 12, 49, 99, 65_000] {
            assert_eq!(unit(number), "", "{number}");
        }
    }
}
