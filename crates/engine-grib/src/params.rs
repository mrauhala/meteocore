/// ECMWF parameter metadata: short name → (long name, unit).
///
/// This covers the ~50 parameters in the ECMWF IFS open data.
/// Full WMO GRIB2 parameter tables have thousands of entries; this is
/// a curated subset for the most commonly used fields.
pub fn ecmwf_param_info(short_name: &str) -> (&'static str, &'static str) {
    match short_name {
        // Surface / single-level
        "2t" => ("2 metre temperature", "K"),
        "2d" => ("2 metre dewpoint temperature", "K"),
        "10u" => ("10 metre U wind component", "m/s"),
        "10v" => ("10 metre V wind component", "m/s"),
        "100u" => ("100 metre U wind component", "m/s"),
        "100v" => ("100 metre V wind component", "m/s"),
        "msl" => ("Mean sea level pressure", "Pa"),
        "sp" => ("Surface pressure", "Pa"),
        "skt" => ("Skin temperature", "K"),
        "tp" => ("Total precipitation", "m"),
        "tprate" => ("Total precipitation rate", "kg m-2 s-1"),
        "cp" => ("Convective precipitation", "m"),
        "sf" => ("Snowfall", "m of water equivalent"),
        "sd" => ("Snow depth", "m of water equivalent"),
        "tcc" => ("Total cloud cover", "0-1"),
        "lcc" => ("Low cloud cover", "0-1"),
        "mcc" => ("Medium cloud cover", "0-1"),
        "hcc" => ("High cloud cover", "0-1"),
        "tcwv" => ("Total column water vapour", "kg m-2"),
        "tcw" => ("Total column water", "kg m-2"),
        "cape" => ("Convective available potential energy", "J/kg"),
        "mucape" => ("Most unstable CAPE", "J/kg"),
        "ptype" => ("Precipitation type", "code"),
        "lsm" => ("Land-sea mask", "0-1"),
        "z" => ("Geopotential", "m2 s-2"),
        "sdor" => ("Standard deviation of orography", "m"),
        "slor" => ("Slope of sub-gridscale orography", "dimensionless"),
        // Radiation (accumulated)
        "ssrd" => ("Surface solar radiation downwards", "J m-2"),
        "strd" => ("Surface thermal radiation downwards", "J m-2"),
        "ssr" => ("Surface net solar radiation", "J m-2"),
        "str" => ("Surface net thermal radiation", "J m-2"),
        "ttr" => ("Top net thermal radiation", "J m-2"),
        // Surface stress
        "ewss" => ("Eastward turbulent surface stress", "N m-2 s"),
        "nsss" => ("Northward turbulent surface stress", "N m-2 s"),
        // Pressure levels
        "t" => ("Temperature", "K"),
        "u" => ("U component of wind", "m/s"),
        "v" => ("V component of wind", "m/s"),
        "q" => ("Specific humidity", "kg/kg"),
        "r" => ("Relative humidity", "%"),
        "gh" => ("Geopotential height", "gpm"),
        "w" => ("Vertical velocity", "Pa/s"),
        "vo" => ("Vorticity (relative)", "s-1"),
        "d" => ("Divergence", "s-1"),
        // Soil
        "sot" => ("Soil temperature", "K"),
        "vsw" => ("Volumetric soil water", "m3 m-3"),
        // Fallback
        _ => ("Unknown parameter", "unknown"),
    }
}
