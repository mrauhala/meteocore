//! KMA GK2A AMI L1B files (`provider = "gk2a"`, #819): NetCDF-4, but not
//! CF. The grid comes from CGMS navigation attributes and the values are
//! instrument counts with quality flags, calibrated here to brightness
//! temperature from the file's own coefficients.
//!
//! A file holds one channel of one scan: `ushort image_pixel_values(y, x)`.
//! Each 16-bit word carries a quality flag in its top
//! `number_of_data_quality_flag_bits_per_pixel` bits (0 good,
//! 1 conditionally usable, 2 outside the scan, 3 error; space is 0x8000) and
//! the count (DN) in its low `number_of_valid_bits_per_pixel` bits. Only
//! good pixels are served, as satpy's `ami_l1b` does by default.

use ds_core::geo::{Crs, GeoTransform, SweepAxis};
use netcdf_reader::{NcFile, NcVariable};

/// Stored integers of a calibrated scan are centi-kelvin: brightness
/// temperature × 100, served through the linear packing of every other
/// product. Half a step, 0.005 K, is below the instrument's noise and the
/// warmest DN step (~0.012 K).
pub(crate) const KELVIN_SCALE: f64 = 0.01;

/// The stored integer of a pixel with no value: a bad quality flag, space,
/// or a count past the calibration's cold end (non-positive radiance).
pub(crate) const MISSING: u16 = u16::MAX;

/// An AMI channel this engine serves.
#[derive(Debug)]
pub(crate) struct Channel {
    /// AMI band number, the product's `band` in the config.
    pub band: u8,
    /// `channel_name` of the file's field.
    pub name: &'static str,
    /// The channel and the resolution code as file names spell them.
    pub file_channel: &'static str,
    pub file_resolution: &'static str,
    /// Central wavenumber (cm⁻¹) the Planck function is inverted at.
    pub wavenumber: f64,
}

/// The AMI channels served.
///
/// Inverting the Planck function needs the channel's central wavenumber,
/// which the files do not carry: `channel_center_wavelength` is the
/// channel's name ("10.5"), and inverting there reads 1.2–1.65 K too cold.
/// IR105's 966.153383926055 cm⁻¹ is the value of the KMA "GK-2A AMI L1B Data
/// User Manual", as NRL GeoIPS quotes it (`ami_netcdf.py`,
/// `CENTER_WAVENUMBERS`). Two checks:
/// - satpy's `ami_l1b` channel table (10.35 µm, 966.18 cm⁻¹) agrees to
///   0.004 K;
/// - with a real file's own coefficients it reproduces KMA's broadcast IR105
///   calibration table (DN 0, 1024, … 7168 → 330.05254 … 220.17763 K) to
///   1e-5 K (`tests::matches_kma_broadcast_table`).
///
/// The manual's other IR channels differ from satpy's rounded wavelengths
/// by 0.05–0.8 K, so they wait for a first-hand source
/// (`ds_core::config::GK2A_BANDS` refuses them at load).
pub(crate) const CHANNELS: [Channel; 1] = [Channel {
    band: 13,
    name: "IR105",
    file_channel: "ir105",
    file_resolution: "020",
    wavenumber: 966.153383926055,
}];

/// The channel of AMI `band`, when served.
pub(crate) fn channel(band: u8) -> Option<&'static Channel> {
    CHANNELS.iter().find(|c| c.band == band)
}

/// DN → brightness temperature from a file's coefficients: radiance
/// `L = gain · DN + offset` (mW m⁻² sr⁻¹ (cm⁻¹)⁻¹), the effective
/// temperature `Teff = (h c ν / k) / ln(2 h c² ν³ / L + 1)` (SI, ν in m⁻¹,
/// L × 1e-5), and `Tbb = c0 + c1 Teff + c2 Teff²`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Calibration {
    gain: f64,
    offset: f64,
    c0: f64,
    c1: f64,
    c2: f64,
    light_speed: f64,
    planck: f64,
    boltzmann: f64,
    /// Central wavenumber, cm⁻¹.
    wavenumber: f64,
}

impl Calibration {
    /// The brightness temperature (K) of count `dn`; `None` where the
    /// radiance is not positive (the cold end of the count range).
    pub fn kelvin(&self, dn: u16) -> Option<f64> {
        let radiance = self.gain * dn as f64 + self.offset;
        if radiance.is_nan() || radiance <= 0.0 {
            return None;
        }
        let (c, h, k) = (self.light_speed, self.planck, self.boltzmann);
        let wn = self.wavenumber * 100.0;
        let t_eff =
            (h * c * wn / k) / (2.0 * h * c * c * wn.powi(3) / (radiance * 1e-5) + 1.0).ln();
        let t = self.c0 + self.c1 * t_eff + self.c2 * t_eff * t_eff;
        t.is_finite().then_some(t)
    }
}

/// A scan's words → stored centi-kelvin: 2^`number_of_valid_bits` entries,
/// built once per scan from its own calibration (8192 for IR105).
#[derive(Debug, PartialEq)]
pub(crate) struct Counts {
    /// Words with any bit at or above this one flagged are not good.
    flag_shift: u32,
    dn_mask: u16,
    table: Box<[u16]>,
}

impl Counts {
    fn new(calibration: &Calibration, valid_bits: u32, flag_bits: u32) -> Result<Counts, String> {
        if !(1..=14).contains(&valid_bits) || !(1..=2).contains(&flag_bits) {
            return Err(format!(
                "{valid_bits} count bits and {flag_bits} quality bits per pixel are not an AMI word"
            ));
        }
        let table = (0..1u32 << valid_bits)
            .map(|dn| {
                calibration
                    .kelvin(dn as u16)
                    .map(|t| (t / KELVIN_SCALE).round())
                    .filter(|&centi| (0.0..MISSING as f64).contains(&centi))
                    .map_or(MISSING, |centi| centi as u16)
            })
            .collect();
        Ok(Counts {
            flag_shift: 16 - flag_bits,
            dn_mask: ((1u32 << valid_bits) - 1) as u16,
            table,
        })
    }

    /// The stored integer of `word`: [`MISSING`] unless its quality is
    /// good.
    pub fn stored(&self, word: u16) -> u16 {
        if word >> self.flag_shift != 0 {
            return MISSING;
        }
        self.table[(word & self.dn_mask) as usize]
    }

    /// Bytes held, for the frame cache.
    pub fn weight(&self) -> u64 {
        self.table.len() as u64 * 2
    }
}

/// The grid and the calibrated counts of `variable` in an AMI L1B file.
pub(crate) fn open(nc: &NcFile, variable: &str) -> Result<(GeoTransform, Counts), String> {
    let var = nc
        .variable(variable)
        .map_err(|e| format!("variable '{variable}': {e}"))?;
    let dims = var.dimensions();
    if dims.len() != 2 {
        return Err(format!(
            "variable '{variable}' has {} dimensions, expected (y, x)",
            dims.len()
        ));
    }
    let size = |i: usize| {
        u32::try_from(dims[i].size)
            .map_err(|_| format!("dimension of {} is too large", dims[i].size))
    };
    let gt = grid(nc, size(1)?, size(0)?)?;
    Ok((gt, counts(nc, var)?))
}

fn number(nc: &NcFile, name: &str) -> Result<f64, String> {
    nc.global_attribute(name)
        .ok()
        .and_then(|a| a.value.as_f64())
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("AMI file has no numeric '{name}'"))
}

/// The grid from the file's CGMS navigation (`cfac`, `lfac`, `coff`,
/// `loff`, numbered from 1) and projection attributes:
/// - `sub_longitude` is in radians (2.2375 = 128.2°E);
/// - `nominal_satellite_height` is the orbit radius, from the Earth's
///   centre (42164 km), so the height above the ellipsoid subtracts
///   `earth_equatorial_radius`;
/// - the angles are CGMS's, PROJ's sweep y (KMA's navigation code is the
///   CGMS formulas).
///
/// KMA stores `lfac` negated on its north-up images, so the orientation
/// comes from the corner scan angles the file states (`image_upperleft_*`,
/// `image_lowerright_*`, radians, positive east and north), and the grid
/// must put its first and last pixel centres on them.
fn grid(nc: &NcFile, nx: u32, ny: u32) -> Result<GeoTransform, String> {
    let n = |name: &str| number(nc, name);
    let projection = nc
        .global_attribute("projection_type")
        .ok()
        .and_then(|a| a.value.as_string());
    if projection.as_deref().map(str::trim) != Some("GEOS") {
        return Err(format!("AMI projection_type {projection:?} is not GEOS"));
    }
    for (name, size) in [("number_of_columns", nx), ("number_of_lines", ny)] {
        if n(name)? != size as f64 {
            return Err(format!("AMI {name} disagrees with the image size {size}"));
        }
    }
    let lon0 = n("sub_longitude")?;
    if lon0.abs() > std::f64::consts::PI {
        return Err(format!("AMI sub_longitude {lon0} is not in radians"));
    }
    let (a, b) = (n("earth_equatorial_radius")?, n("earth_polar_radius")?);
    let height = n("nominal_satellite_height")? - a;
    if !(a >= b && b > 0.0 && height > 0.0) {
        return Err(format!(
            "AMI earth radii {a}, {b} and orbit height {height} are not usable"
        ));
    }
    let crs = Crs::Geostationary {
        lon0,
        height,
        semi_major: a,
        semi_minor: b,
        sweep: SweepAxis::Y,
    };
    let [ul_x, ul_y, lr_x, lr_y] = [
        n("image_upperleft_x")?,
        n("image_upperleft_y")?,
        n("image_lowerright_x")?,
        n("image_lowerright_y")?,
    ];
    if !(ul_x < lr_x && ul_y > lr_y) {
        return Err("AMI image is not north up with west on the left".to_string());
    }
    let gt = GeoTransform::from_cgms(
        n("cfac")?.abs(),
        n("lfac")?.abs(),
        n("coff")?,
        n("loff")?,
        nx,
        ny,
        crs,
    )?;
    // The stated corners (to 1e-6 rad) are the pixel centres, to a tenth of
    // a pixel: a numbering from 0 or a wrong sign is a whole pixel off.
    let centre = |col: u32, row: u32| {
        (
            (gt.origin_x + (col as f64 + 0.5) * gt.pixel_width) / height,
            (gt.origin_y - (row as f64 + 0.5) * gt.pixel_height) / height,
        )
    };
    let tolerance = 0.1 * gt.pixel_width / height;
    for ((x, y), (want_x, want_y)) in [
        (centre(0, 0), (ul_x, ul_y)),
        (centre(nx - 1, ny - 1), (lr_x, lr_y)),
    ] {
        if (x - want_x).abs() > tolerance || (y - want_y).abs() > tolerance {
            return Err(format!(
                "AMI navigation puts a corner pixel at ({x:.6}, {y:.6}) rad, the file states \
                 ({want_x}, {want_y})"
            ));
        }
    }
    Ok(gt)
}

/// The calibration of `var`: its channel's wavenumber (see [`CHANNELS`]),
/// the file's DN → radiance and Teff → Tbb coefficients and constants, and
/// its word layout.
fn counts(nc: &NcFile, var: &NcVariable) -> Result<Counts, String> {
    let n = |name: &str| number(nc, name);
    let channel_name = var
        .attribute("channel_name")
        .and_then(|a| a.value.as_string())
        .unwrap_or_default();
    let channel = CHANNELS
        .iter()
        .find(|c| c.name == channel_name.trim())
        .ok_or_else(|| format!("AMI channel '{channel_name}' has no sourced central wavenumber"))?;
    let calibration = Calibration {
        gain: n("DN_to_Radiance_Gain")?,
        offset: n("DN_to_Radiance_Offset")?,
        c0: n("Teff_to_Tbb_c0")?,
        c1: n("Teff_to_Tbb_c1")?,
        c2: n("Teff_to_Tbb_c2")?,
        light_speed: n("light_speed")?,
        planck: n("Plank_constant_h")?,
        boltzmann: n("Boltzmann_constant_k")?,
        wavenumber: channel.wavenumber,
    };
    let bits = |name: &str| {
        var.attribute(name)
            .and_then(|a| a.value.as_f64())
            .filter(|v| v.fract() == 0.0 && (0.0..=16.0).contains(v))
            .map(|v| v as u32)
            .ok_or_else(|| format!("AMI field has no '{name}'"))
    };
    if bits("number_of_total_bits_per_pixel")? != 16 {
        return Err("AMI words are not 16 bits".to_string());
    }
    Counts::new(
        &calibration,
        bits("number_of_valid_bits_per_pixel")?,
        bits("number_of_data_quality_flag_bits_per_pixel")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> NcFile {
        NcFile::open(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/gk2a-ami/gk2a_ami_le1b_ir105_fd020ge_202609281200.nc"),
        )
        .unwrap()
    }

    fn calibration(nc: &NcFile, wavenumber: f64) -> Calibration {
        let n = |name: &str| number(nc, name).unwrap();
        Calibration {
            gain: n("DN_to_Radiance_Gain"),
            offset: n("DN_to_Radiance_Offset"),
            c0: n("Teff_to_Tbb_c0"),
            c1: n("Teff_to_Tbb_c1"),
            c2: n("Teff_to_Tbb_c2"),
            light_speed: n("light_speed"),
            planck: n("Plank_constant_h"),
            boltzmann: n("Boltzmann_constant_k"),
            wavenumber,
        }
    }

    /// KMA broadcasts each IR image's calibration with it: IR105's is
    /// DN 0, 1024, … 7168 → 330.05254, 319.99371, 309.08976, 297.08050,
    /// 283.54929, 267.75568, 248.14399, 220.17763 K (as received from the
    /// GK-2A LRIT downlink, vksdr.com "GK-2A IR Colour Enhancement"). The
    /// fixture's own coefficients at the manual's wavenumber reproduce it;
    /// at the file's nominal 10.5 µm they are 1.4–1.65 K too cold.
    #[test]
    fn matches_kma_broadcast_table() {
        let nc = fixture();
        let kma = [
            330.05254, 319.99371, 309.08976, 297.08050, 283.54929, 267.75568, 248.14399, 220.17763,
        ];
        let served = calibration(&nc, CHANNELS[0].wavenumber);
        let nominal = calibration(&nc, 1e4 / 10.5);
        for (i, want) in kma.into_iter().enumerate() {
            let dn = i as u16 * 1024;
            let t = served.kelvin(dn).unwrap();
            assert!((t - want).abs() < 1e-5, "DN {dn}: {t} vs {want}");
            let off = nominal.kelvin(dn).unwrap() - want;
            assert!(
                (-1.66..-1.39).contains(&off),
                "DN {dn}: 10.5 µm is {off} K off"
            );
        }
        // Radiance reaches zero at DN 8152.5: colder counts have no value.
        assert!(served
            .kelvin(8152)
            .is_some_and(|t| (t - 100.001353).abs() < 1e-5));
        assert_eq!(served.kelvin(8153), None);
    }

    /// Words: good ones calibrate through the table (to 0.005 K), any
    /// quality flag is no value (0x8000 is space), and the unused bit 13 of
    /// a 13-bit channel is ignored.
    #[test]
    fn words_decode_through_the_table_and_quality_flags() {
        let nc = fixture();
        let var = nc.variable("image_pixel_values").unwrap();
        let counts = counts(&nc, var).unwrap();
        assert_eq!(counts.table.len(), 8192);
        let exact = calibration(&nc, CHANNELS[0].wavenumber);
        for dn in [0u16, 3147, 3286, 4746, 8152] {
            let stored = counts.stored(dn);
            let t = exact.kelvin(dn).unwrap();
            assert!(
                (stored as f64 * KELVIN_SCALE - t).abs() <= 0.005 + 1e-9,
                "DN {dn}"
            );
            assert_eq!(counts.stored(dn | 1 << 13), stored);
        }
        for word in [0x8000, 0x4000 | 3286, 0xC000 | 3286, 8153, 8191] {
            assert_eq!(counts.stored(word), MISSING, "{word:#06x}");
        }
    }

    /// The fixture's grid is its window of the full disk: column 1 is
    /// full-disk column 4914 (COFF −2162.5), line 1 line 1946 (LOFF 805.5),
    /// on the corner angles the file states.
    #[test]
    fn grid_is_the_window_of_the_full_disk() {
        let nc = fixture();
        let (gt, _) = open(&nc, "image_pixel_values").unwrap();
        assert_eq!((gt.width, gt.height), (160, 96));
        let full = GeoTransform::from_cgms(
            20_425_338.903_339_4,
            20_425_338.903_339_4,
            2750.5,
            2750.5,
            5500,
            5500,
            gt.crs.clone(),
        )
        .unwrap();
        let eps = 1e-6 * gt.pixel_width;
        assert!((gt.origin_x - (full.origin_x + 4913.0 * full.pixel_width)).abs() < eps);
        assert!((gt.origin_y - (full.origin_y - 1945.0 * full.pixel_height)).abs() < eps);
        let Crs::Geostationary { lon0, height, .. } = gt.crs else {
            panic!("{:?}", gt.crs)
        };
        assert!((lon0.to_degrees() - 128.2).abs() < 1e-9);
        assert_eq!(height, 35_785_863.0);
    }
}
