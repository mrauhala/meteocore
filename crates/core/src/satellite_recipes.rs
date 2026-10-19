//! Built-in RGB composite recipes for satellite collections (#819 phase 4).
//!
//! `recipe = "airmass"` in a `[[satellite.composites]]` entry stands for the
//! red, green and blue channels written out: each channel reads a band, or
//! the difference of two, by the instrument's band number, and
//! [`crate::config::satellite_composite_def`] fills in the product parameter
//! that has that `band`. This table is the only home of the coefficients.
//!
//! A recipe is written per instrument, because the recipes are adapted to
//! each imager's bands. ABI (GOES-R) and AHI (Himawari) share the band
//! numbers used here: 7 = 3.9 µm, 8 = 6.2 µm, 10 = 7.3 µm, 12 = 9.6 µm,
//! 13 = 10.3/10.4 µm, 14 = 11.2 µm, 15 = 12.3/12.4 µm.
//!
//! Ranges follow `ds_render::composite`: `min` gives intensity 0 and `max`
//! full intensity, so `min > max` inverts a channel, as satpy's `crude`
//! stretch reads `min_stretch` and `max_stretch`. Every gamma is 1: no
//! source below applies another.
//!
//! # Sources
//!
//! The values are satpy's (pytroll/satpy `main`, commit `06f4a0e`, read
//! 2026-09-29), bands from the composite files and stretches from the
//! enhancement files:
//!
//! - <https://github.com/pytroll/satpy/blob/main/satpy/etc/composites/abi.yaml>
//!   `airmass`, `night_microphysics`
//! - <https://github.com/pytroll/satpy/blob/main/satpy/etc/composites/ahi.yaml>
//!   `airmass`, `night_microphysics`
//! - <https://github.com/pytroll/satpy/blob/main/satpy/etc/enhancements/abi.yaml>
//!   `airmass`, `night_microphysics_abi`
//! - <https://github.com/pytroll/satpy/blob/main/satpy/etc/enhancements/ahi.yaml>
//!   `airmass` ("matches ABI")
//! - <https://github.com/pytroll/satpy/blob/main/satpy/etc/enhancements/generic.yaml>
//!   `night_microphysics_default`, which AHI uses because `ahi.yaml` has no
//!   Night Microphysics stretch of its own
//!
//! The agency quick guides agree for ABI and differ for AHI:
//!
//! - ABI matches the CIRA/RAMMB GOES-R quick guides exactly. They give the
//!   single-band ranges in °C: Airmass blue −29.25 to −64.65 °C is 243.9 to
//!   208.5 K, Night Microphysics blue −29.6 to 19.5 °C is 243.55 to
//!   292.65 K.
//!   <https://rammb.cira.colostate.edu/training/visit/quick_guides/QuickGuide_GOESR_AirMassRGB_final.pdf>,
//!   <https://rammb.cira.colostate.edu/training/visit/quick_guides/QuickGuide_GOESR_NtMicroRGB_Final_20191206.pdf>
//! - For AHI, satpy reads band 14 (11.2 µm) where JMA's Himawari quick
//!   guides read band 13 (10.4 µm), and its Night Microphysics keeps
//!   EUMETSAT's SEVIRI ranges. JMA writes Airmass as B10 − B08 over 0 to
//!   25.8 K, B13 − B12 over −4.3 to 41.5 K and B08 over 208.0 to 242.6 K,
//!   and Night Microphysics as B13 − B15 over −3.0 to 7.5 K, B07 − B13 over
//!   −7.0 to 2.9 K and B13 over 243.7 to 293.2 K. The table keeps satpy's
//!   AHI values.
//!   <https://www.jma.go.jp/jma/jma-eng/satellite/VLab/QG/RGB_QG_Airmass_en.pdf>,
//!   <https://www.jma.go.jp/jma/jma-eng/satellite/VLab/QG/RGB_QG_NightMicrophysics_en.pdf>

/// An imager whose band numbers a recipe is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instrument {
    /// GOES-R Advanced Baseline Imager.
    Abi,
    /// Himawari Advanced Himawari Imager.
    Ahi,
}

impl Instrument {
    /// The imager whose brightness temperatures a `[satellite].provider`
    /// serves, or `None` for a provider whose values recipes cannot read.
    /// GMGSI (phase 3) must stay `None`: its values are 8-bit display
    /// counts, not Kelvin.
    pub fn of_provider(provider: &str) -> Option<Instrument> {
        match provider {
            "goes-r" => Some(Instrument::Abi),
            "isatss" => Some(Instrument::Ahi),
            _ => None,
        }
    }

    /// The instrument's name in messages: `"ABI"` or `"AHI"`.
    pub fn name(self) -> &'static str {
        match self {
            Instrument::Abi => "ABI",
            Instrument::Ahi => "AHI",
        }
    }
}

/// One channel of a [`Recipe`], by band number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecipeChannel {
    /// The band read, or the minuend of a difference.
    pub band: u8,
    /// The band subtracted from `band`.
    pub minus: Option<u8>,
    /// Brightness temperature, or difference, in K that gives intensity 0.
    pub min: f64,
    /// The value that gives full intensity. `min > max` inverts.
    pub max: f64,
    /// `intensity = stretch ^ (1 / gamma)`.
    pub gamma: f64,
}

impl RecipeChannel {
    /// The bands this channel reads, minuend first.
    pub fn bands(&self) -> impl Iterator<Item = u8> {
        std::iter::once(self.band).chain(self.minus)
    }
}

/// A built-in RGB composite recipe for one instrument.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recipe {
    /// The `recipe` value, e.g. `"airmass"`.
    pub name: &'static str,
    /// The composite's default title.
    pub title: &'static str,
    pub instrument: Instrument,
    /// Red, green and blue.
    pub channels: [RecipeChannel; 3],
}

impl Recipe {
    /// The distinct bands the recipe reads, ascending.
    pub fn bands(&self) -> Vec<u8> {
        let mut bands: Vec<u8> = self
            .channels
            .iter()
            .flat_map(RecipeChannel::bands)
            .collect();
        bands.sort_unstable();
        bands.dedup();
        bands
    }
}

/// `band − minus` over `min..max`, gamma 1.
const fn difference(band: u8, minus: u8, min: f64, max: f64) -> RecipeChannel {
    RecipeChannel {
        band,
        minus: Some(minus),
        min,
        max,
        gamma: 1.0,
    }
}

/// `band` over `min..max`, gamma 1.
const fn single(band: u8, min: f64, max: f64) -> RecipeChannel {
    RecipeChannel {
        band,
        minus: None,
        min,
        max,
        gamma: 1.0,
    }
}

const AIRMASS_TITLE: &str = "Airmass RGB";
const NIGHT_MICROPHYSICS_TITLE: &str = "Night Microphysics RGB";

/// Every built-in recipe, one entry per recipe and instrument. See the
/// module docs for the sources.
pub static RECIPES: [Recipe; 4] = [
    // ABI: satpy composites/abi.yaml `airmass`, enhancements/abi.yaml
    // `airmass`; CIRA/RAMMB Air Mass RGB quick guide.
    Recipe {
        name: "airmass",
        title: AIRMASS_TITLE,
        instrument: Instrument::Abi,
        channels: [
            difference(8, 10, -26.2, 0.6),
            difference(12, 13, -43.2, 6.7),
            single(8, 243.9, 208.5),
        ],
    },
    // ABI: satpy composites/abi.yaml `night_microphysics`,
    // enhancements/abi.yaml `night_microphysics_abi`; CIRA/RAMMB NtMicro
    // RGB quick guide.
    Recipe {
        name: "night_microphysics",
        title: NIGHT_MICROPHYSICS_TITLE,
        instrument: Instrument::Abi,
        channels: [
            difference(15, 13, -6.7, 2.6),
            difference(13, 7, -3.1, 5.2),
            single(13, 243.55, 292.65),
        ],
    },
    // AHI: satpy composites/ahi.yaml `airmass` (green reads band 14),
    // enhancements/ahi.yaml `airmass`, the ABI stretch.
    Recipe {
        name: "airmass",
        title: AIRMASS_TITLE,
        instrument: Instrument::Ahi,
        channels: [
            difference(8, 10, -26.2, 0.6),
            difference(12, 14, -43.2, 6.7),
            single(8, 243.9, 208.5),
        ],
    },
    // AHI: satpy composites/ahi.yaml `night_microphysics` (green reads band
    // 14), enhancements/generic.yaml `night_microphysics_default`, the
    // EUMETSAT SEVIRI stretch.
    Recipe {
        name: "night_microphysics",
        title: NIGHT_MICROPHYSICS_TITLE,
        instrument: Instrument::Ahi,
        channels: [
            difference(15, 13, -4.0, 2.0),
            difference(14, 7, 0.0, 10.0),
            single(13, 243.0, 293.0),
        ],
    },
];

/// The recipe `name` for `instrument`.
pub fn recipe(name: &str, instrument: Instrument) -> Option<&'static Recipe> {
    RECIPES
        .iter()
        .find(|r| r.name == name && r.instrument == instrument)
}

/// The recipe names `instrument` has, in table order.
pub fn recipe_names(instrument: Instrument) -> Vec<&'static str> {
    RECIPES
        .iter()
        .filter(|r| r.instrument == instrument)
        .map(|r| r.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel as `(band, minus, min, max, gamma)`.
    fn spec(channel: &RecipeChannel) -> (u8, Option<u8>, f64, f64, f64) {
        (
            channel.band,
            channel.minus,
            channel.min,
            channel.max,
            channel.gamma,
        )
    }

    fn channels(name: &str, instrument: Instrument) -> [(u8, Option<u8>, f64, f64, f64); 3] {
        recipe(name, instrument)
            .unwrap_or_else(|| panic!("{name} for {instrument:?}"))
            .channels
            .each_ref()
            .map(spec)
    }

    /// The cited values, pinned channel by channel: a table edit that
    /// changes a picture fails here first.
    #[test]
    fn recipes_carry_the_cited_values() {
        assert_eq!(
            channels("airmass", Instrument::Abi),
            [
                (8, Some(10), -26.2, 0.6, 1.0),
                (12, Some(13), -43.2, 6.7, 1.0),
                (8, None, 243.9, 208.5, 1.0),
            ]
        );
        assert_eq!(
            channels("night_microphysics", Instrument::Abi),
            [
                (15, Some(13), -6.7, 2.6, 1.0),
                (13, Some(7), -3.1, 5.2, 1.0),
                (13, None, 243.55, 292.65, 1.0),
            ]
        );
        assert_eq!(
            channels("airmass", Instrument::Ahi),
            [
                (8, Some(10), -26.2, 0.6, 1.0),
                (12, Some(14), -43.2, 6.7, 1.0),
                (8, None, 243.9, 208.5, 1.0),
            ]
        );
        assert_eq!(
            channels("night_microphysics", Instrument::Ahi),
            [
                (15, Some(13), -4.0, 2.0, 1.0),
                (14, Some(7), 0.0, 10.0, 1.0),
                (13, None, 243.0, 293.0, 1.0),
            ]
        );
        let title = |name, instrument| recipe(name, instrument).unwrap().title;
        for instrument in [Instrument::Abi, Instrument::Ahi] {
            assert_eq!(title("airmass", instrument), "Airmass RGB");
            assert_eq!(
                title("night_microphysics", instrument),
                "Night Microphysics RGB"
            );
        }
        assert_eq!(
            recipe("airmass", Instrument::Abi).unwrap().bands(),
            [8, 10, 12, 13]
        );
        assert_eq!(
            recipe("night_microphysics", Instrument::Ahi)
                .unwrap()
                .bands(),
            [7, 13, 14, 15]
        );
    }

    /// Both instruments have every recipe, once, and each channel passes
    /// the checks `validate_satellite` applies to written-out channels.
    #[test]
    fn the_table_is_complete_and_well_formed() {
        for instrument in [Instrument::Abi, Instrument::Ahi] {
            assert_eq!(recipe_names(instrument), ["airmass", "night_microphysics"]);
        }
        assert!(recipe("airmass", Instrument::Abi).is_some());
        assert!(recipe("true_color", Instrument::Abi).is_none());
        for r in &RECIPES {
            for channel in &r.channels {
                assert!(channel.bands().all(|b| (1..=16).contains(&b)), "{r:?}");
                assert_ne!(channel.minus, Some(channel.band), "{r:?}");
                assert!(channel.min.is_finite() && channel.max.is_finite());
                assert_ne!(channel.min, channel.max, "{r:?}");
                assert!(channel.gamma.is_finite() && channel.gamma > 0.0);
            }
        }
    }

    #[test]
    fn providers_map_to_instruments() {
        assert_eq!(Instrument::of_provider("goes-r"), Some(Instrument::Abi));
        assert_eq!(Instrument::of_provider("isatss"), Some(Instrument::Ahi));
        assert_eq!(Instrument::of_provider("gmgsi"), None);
        assert_eq!(Instrument::Abi.name(), "ABI");
        assert_eq!(Instrument::Ahi.name(), "AHI");
    }
}
