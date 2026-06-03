//! ODIM_H5 quantity dictionary.
//!
//! Maps bare ODIM quantity codes (`DBZH`, `VRADH`, `ZDR`, …) to a
//! human-readable name and canonical unit, per the EUMETNET OPERA ODIM_H5
//! specification quantity table. The bare code stays the parameter *id*
//! (URL short-name, WMS `<Name>`, CoverageJSON parameter key) everywhere;
//! this table only supplies the *display label* and the unit. Unknown codes
//! fall back to the bare string so a quantity the table doesn't know about is
//! still served, just without a prettier label.

/// `(human name, canonical unit)` for an ODIM quantity code, or `None` when the
/// code is not in the table. The unit is the physical unit the quantity is
/// expressed in after ODIM `gain`/`offset` are applied; an empty string marks a
/// dimensionless quantity (e.g. correlation coefficient, quality index).
pub fn quantity_info(quantity: &str) -> Option<(&'static str, &'static str)> {
    Some(match quantity {
        "TH" => ("Total reflectivity (horizontal)", "dBZ"),
        "TV" => ("Total reflectivity (vertical)", "dBZ"),
        "DBZH" => ("Reflectivity (horizontal)", "dBZ"),
        "DBZV" => ("Reflectivity (vertical)", "dBZ"),
        "DBZ" => ("Reflectivity", "dBZ"),
        "ZDR" => ("Differential reflectivity", "dB"),
        "RHOHV" => ("Correlation coefficient", ""), // unitless, 0–1
        "LDR" => ("Linear depolarization ratio", "dB"),
        "PHIDP" => ("Differential phase", "deg"),
        "KDP" => ("Specific differential phase", "deg/km"),
        "SQIH" => ("Signal quality index (horizontal)", ""),
        "SQIV" => ("Signal quality index (vertical)", ""),
        "SNRH" => ("Signal-to-noise ratio (horizontal)", "dB"),
        "SNRV" => ("Signal-to-noise ratio (vertical)", "dB"),
        "VRADH" => ("Radial velocity (horizontal)", "m/s"),
        "VRADV" => ("Radial velocity (vertical)", "m/s"),
        "VRAD" => ("Radial velocity", "m/s"),
        "WRADH" => ("Spectral width (horizontal)", "m/s"),
        "WRADV" => ("Spectral width (vertical)", "m/s"),
        "WRAD" => ("Spectral width", "m/s"),
        "QIND" => ("Quality index", ""),
        "RATE" => ("Precipitation rate", "mm/h"),
        "ACRR" => ("Accumulated precipitation", "mm"),
        "HGHT" => ("Height", "km"),
        "VIL" => ("Vertically integrated liquid", "kg/m²"),
        _ => return None,
    })
}

/// Display label in `"CODE — Name"` form, e.g. `"DBZH — Reflectivity
/// (horizontal)"`. Unknown codes fall back to the bare code with underscores
/// turned into spaces, preserving the engine's previous behaviour.
pub fn quantity_label(quantity: &str) -> String {
    match quantity_info(quantity) {
        Some((name, _)) => format!("{quantity} — {name}"),
        None => quantity.replace('_', " "),
    }
}

/// Canonical unit for a quantity, or `""` when the code is unknown or the
/// quantity is dimensionless.
pub fn quantity_unit(quantity: &str) -> &'static str {
    quantity_info(quantity).map(|(_, unit)| unit).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_quantities_get_name_and_unit() {
        assert_eq!(quantity_label("DBZH"), "DBZH — Reflectivity (horizontal)");
        assert_eq!(quantity_unit("DBZH"), "dBZ");
        assert_eq!(
            quantity_label("VRADH"),
            "VRADH — Radial velocity (horizontal)"
        );
        assert_eq!(quantity_unit("VRADH"), "m/s");
    }

    #[test]
    fn dimensionless_quantity_has_empty_unit() {
        assert_eq!(quantity_label("RHOHV"), "RHOHV — Correlation coefficient");
        assert_eq!(quantity_unit("RHOHV"), "");
    }

    #[test]
    fn unknown_quantity_falls_back_to_bare_code() {
        assert_eq!(quantity_label("ZZZZ"), "ZZZZ");
        assert_eq!(quantity_label("FOO_BAR"), "FOO BAR");
        assert_eq!(quantity_unit("ZZZZ"), "");
        assert!(quantity_info("ZZZZ").is_none());
    }
}
