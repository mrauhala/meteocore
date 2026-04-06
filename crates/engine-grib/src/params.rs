/// ECMWF parameter metadata: short name → (long name, source unit, display unit, scale, offset).
///
/// This covers the ~50 parameters in the ECMWF IFS open data.
/// Full WMO GRIB2 parameter tables have thousands of entries; this is
/// a curated subset for the most commonly used fields.
///
/// Unit conversions are applied automatically:
/// - Temperature: K → °C (offset -273.15)
/// - Pressure: Pa → hPa (scale 0.01)
/// - Precipitation depth: m → mm (scale 1000)
/// - Geopotential: m² s⁻² → gpm (scale 1/9.80665)
pub struct ParamInfo {
    /// Human-readable parameter name.
    pub label: &'static str,
    /// Display unit (after conversion).
    pub unit: &'static str,
    /// Scale factor: display_value = raw * scale + offset.
    pub scale: f64,
    /// Offset: display_value = raw * scale + offset.
    pub offset: f64,
}

impl ParamInfo {
    const fn identity(label: &'static str, unit: &'static str) -> Self {
        Self {
            label,
            unit,
            scale: 1.0,
            offset: 0.0,
        }
    }

    const fn with_conversion(
        label: &'static str,
        unit: &'static str,
        scale: f64,
        offset: f64,
    ) -> Self {
        Self {
            label,
            unit,
            scale,
            offset,
        }
    }

    /// Convert a raw GRIB value to display units.
    #[inline]
    pub fn convert(&self, raw: f64) -> f64 {
        raw * self.scale + self.offset
    }

    /// True if this parameter has a non-identity conversion.
    pub fn has_conversion(&self) -> bool {
        (self.scale - 1.0).abs() > 1e-12 || self.offset.abs() > 1e-12
    }
}

/// Look up parameter info by ECMWF short name.
pub fn ecmwf_param_info(short_name: &str) -> ParamInfo {
    match short_name {
        // Temperature: K → °C
        "2t" => ParamInfo::with_conversion("2 metre temperature", "°C", 1.0, -273.15),
        "2d" => ParamInfo::with_conversion("2 metre dewpoint temperature", "°C", 1.0, -273.15),
        "skt" => ParamInfo::with_conversion("Skin temperature", "°C", 1.0, -273.15),
        "t" => ParamInfo::with_conversion("Temperature", "°C", 1.0, -273.15),
        "sot" => ParamInfo::with_conversion("Soil temperature", "°C", 1.0, -273.15),

        // Pressure: Pa → hPa
        "msl" => ParamInfo::with_conversion("Mean sea level pressure", "hPa", 0.01, 0.0),
        "sp" => ParamInfo::with_conversion("Surface pressure", "hPa", 0.01, 0.0),

        // Precipitation depth: m → mm
        "tp" => ParamInfo::with_conversion("Total precipitation", "mm", 1000.0, 0.0),
        "cp" => ParamInfo::with_conversion("Convective precipitation", "mm", 1000.0, 0.0),
        "sf" => ParamInfo::with_conversion("Snowfall", "mm", 1000.0, 0.0),
        "sd" => ParamInfo::with_conversion("Snow depth", "mm", 1000.0, 0.0),

        // Geopotential → geopotential height: m²s⁻² → gpm (divide by g)
        "z" => ParamInfo::with_conversion("Geopotential height", "gpm", 1.0 / 9.80665, 0.0),

        // Wind: no conversion needed
        "10u" => ParamInfo::identity("10 metre U wind component", "m/s"),
        "10v" => ParamInfo::identity("10 metre V wind component", "m/s"),
        "100u" => ParamInfo::identity("100 metre U wind component", "m/s"),
        "100v" => ParamInfo::identity("100 metre V wind component", "m/s"),
        "u" => ParamInfo::identity("U component of wind", "m/s"),
        "v" => ParamInfo::identity("V component of wind", "m/s"),
        "w" => ParamInfo::identity("Vertical velocity", "Pa/s"),

        // Cloud cover: 0-1 → %
        "tcc" => ParamInfo::with_conversion("Total cloud cover", "%", 100.0, 0.0),
        "lcc" => ParamInfo::with_conversion("Low cloud cover", "%", 100.0, 0.0),
        "mcc" => ParamInfo::with_conversion("Medium cloud cover", "%", 100.0, 0.0),
        "hcc" => ParamInfo::with_conversion("High cloud cover", "%", 100.0, 0.0),
        "q" => ParamInfo::identity("Specific humidity", "kg/kg"),
        "r" => ParamInfo::identity("Relative humidity", "%"),
        "vsw" => ParamInfo::identity("Volumetric soil water", "m³/m³"),

        // Column-integrated quantities: no conversion
        "tcwv" => ParamInfo::identity("Total column water vapour", "kg/m²"),
        "tcw" => ParamInfo::identity("Total column water", "kg/m²"),

        // Energy / accumulated fields: no conversion
        "cape" => ParamInfo::identity("Convective available potential energy", "J/kg"),
        "mucape" => ParamInfo::identity("Most unstable CAPE", "J/kg"),
        "ssrd" => ParamInfo::identity("Surface solar radiation downwards", "J/m²"),
        "strd" => ParamInfo::identity("Surface thermal radiation downwards", "J/m²"),
        "ssr" => ParamInfo::identity("Surface net solar radiation", "J/m²"),
        "str" => ParamInfo::identity("Surface net thermal radiation", "J/m²"),
        "ttr" => ParamInfo::identity("Top net thermal radiation", "J/m²"),

        // Other: no conversion
        "tprate" => ParamInfo::identity("Total precipitation rate", "kg/(m²·s)"),
        "ptype" => ParamInfo::identity("Precipitation type", "code"),
        "lsm" => ParamInfo::with_conversion("Land-sea mask", "%", 100.0, 0.0),
        "sdor" => ParamInfo::identity("Standard deviation of orography", "m"),
        "slor" => ParamInfo::identity("Slope of sub-gridscale orography", "dimensionless"),
        "ewss" => ParamInfo::identity("Eastward turbulent surface stress", "N/(m²·s)"),
        "nsss" => ParamInfo::identity("Northward turbulent surface stress", "N/(m²·s)"),
        "gh" => ParamInfo::identity("Geopotential height", "gpm"),
        "vo" => ParamInfo::identity("Vorticity (relative)", "1/s"),
        "d" => ParamInfo::identity("Divergence", "1/s"),

        // Fallback: no conversion
        _ => ParamInfo::identity("Unknown parameter", "unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kelvin_to_celsius() {
        let info = ecmwf_param_info("2t");
        assert_eq!(info.unit, "°C");
        assert!((info.convert(273.15) - 0.0).abs() < 1e-10);
        assert!((info.convert(293.15) - 20.0).abs() < 1e-10);
    }

    #[test]
    fn pascal_to_hpa() {
        let info = ecmwf_param_info("msl");
        assert_eq!(info.unit, "hPa");
        assert!((info.convert(101325.0) - 1013.25).abs() < 1e-10);
    }

    #[test]
    fn metres_to_mm() {
        let info = ecmwf_param_info("tp");
        assert_eq!(info.unit, "mm");
        assert!((info.convert(0.005) - 5.0).abs() < 1e-10);
    }

    #[test]
    fn identity_no_conversion() {
        let info = ecmwf_param_info("10u");
        assert!(!info.has_conversion());
        assert!((info.convert(5.0) - 5.0).abs() < 1e-10);
    }

    #[test]
    fn fraction_to_percent() {
        let info = ecmwf_param_info("tcc");
        assert_eq!(info.unit, "%");
        assert!((info.convert(0.75) - 75.0).abs() < 1e-10);
        assert!((info.convert(1.0) - 100.0).abs() < 1e-10);
    }

    #[test]
    fn geopotential_to_gpm() {
        let info = ecmwf_param_info("z");
        // 9806.65 m²/s² = 1000 gpm
        assert!((info.convert(9806.65) - 1000.0).abs() < 0.01);
    }
}
