/// Trait for mapping raster values to RGBA colors.
pub trait ColorMap: Send + Sync {
    /// Map a value to an RGBA color. None = nodata → transparent.
    fn color(&self, value: Option<f64>) -> [u8; 4];

    /// Colour returned for `None` / NaN / ±∞ inputs. Defaults to fully transparent.
    /// Overridden by concrete impls that carry an explicit nodata colour so
    /// wrappers like `IntegerLutColorMap::from_colormap` can preserve it.
    fn nodata_color(&self) -> [u8; 4] {
        [0, 0, 0, 0]
    }
}

/// A color stop for defining gradient colormaps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorStop {
    pub value: f64,
    pub color: [u8; 4],
}

/// Built-in colormap names.
///
/// Legacy shim: the authoritative palette data lives in the
/// [`crate::palette`] builtin table; this enum only covers the original 12
/// palettes and exists for `LutColorMap::from_builtin` callers. New code
/// (and the newer builtins like `radial_velocity`) should resolve by name
/// via [`crate::palette::builtin_palette`] or a `PaletteRegistry`.
pub enum BuiltinColormap {
    /// Standard radar reflectivity (blue → green → yellow → red).
    RadarDbz,
    /// SMHI radar reflectivity with gray below-threshold, per-dBZ colors, -29 to 70 dBZ.
    RadarSmhi,
    /// FMI summer radar reflectivity (cyan → green → yellow → orange → red → magenta → pink).
    RadarFmi,
    /// Bookbinder 8-bit Z curve (Evan Bookbinder, WFO SGF). Full dBZ range to 95.
    RadarBookbinder,
    /// Linear grayscale (black → white).
    Grayscale,
    /// Perceptually uniform (dark purple → blue → green → yellow).
    Viridis,
    /// Temperature palette (blue → cyan → green → yellow → red).
    Temperature,
    /// Precipitation accumulation palette (light blue → blue → purple → magenta). 0–50 mm.
    Precipitation,
    /// Precipitation rate palette (transparent → cyan → blue → green → yellow → red). 0–30 mm/h.
    PrecipitationRate,
    /// Wind speed palette (calm green → yellow → orange → red → purple).
    WindSpeed,
    /// CAP (Common Alerting Protocol) severity ramp. Integer severity codes
    /// 0–4 → grey/green/yellow/orange/red with a semi-transparent alpha so the
    /// alert fill overlays a basemap (Unknown=0, Minor=1, Moderate=2, Severe=3,
    /// Extreme=4). Used by the `engine-cap` alert map layers (#396).
    CapSeverity,
    /// Lightning strike-age ramp (#504). Value = strike age in MINUTES:
    /// fresh strikes near-white/yellow, aging through orange and red to a
    /// dark violet at the window edge. Style min/max set the window
    /// (default 0–60 min). Fully opaque symbols — strikes are sparse point
    /// splats over a basemap, not an area fill.
    LightningAge,
}

impl BuiltinColormap {
    /// The palette-table name of this variant (the string accepted in
    /// `colormap = "..."` config).
    pub fn name(&self) -> &'static str {
        match self {
            BuiltinColormap::RadarDbz => "radar_dbz",
            BuiltinColormap::RadarSmhi => "radar_smhi",
            BuiltinColormap::RadarFmi => "radar_fmi",
            BuiltinColormap::RadarBookbinder => "radar_bookbinder",
            BuiltinColormap::Grayscale => "grayscale",
            BuiltinColormap::Viridis => "viridis",
            BuiltinColormap::Temperature => "temperature",
            BuiltinColormap::Precipitation => "precipitation",
            BuiltinColormap::PrecipitationRate => "precipitation_rate",
            BuiltinColormap::WindSpeed => "wind_speed",
            BuiltinColormap::CapSeverity => "cap_severity",
            BuiltinColormap::LightningAge => "lightning_age",
        }
    }
}

/// Lookup-table colormap. O(1) per pixel.
/// Best for integer-like data (radar U8, classification).
pub struct LutColorMap {
    lut: Vec<[u8; 4]>,
    min: f64,
    max: f64,
    nodata_color: [u8; 4],
    /// Color for values strictly below `min`. Probed from the stops one
    /// ULP under `min`, so it equals the clamp color for ordinary
    /// palettes and transparent for palettes carrying a display-threshold
    /// guard stop (the `.pal` importer) — below-minimum data then
    /// disappears exactly like in the source applications.
    underflow_color: [u8; 4],
}

impl LutColorMap {
    /// Create a LUT colormap from a built-in palette.
    ///
    /// The min/max define the value range mapped to the LUT.
    /// Values in the stops define which colors appear at which physical values.
    /// The LUT samples the stops across the min..max range.
    pub fn from_builtin(builtin: BuiltinColormap, min: f64, max: f64) -> Self {
        let palette = crate::palette::builtin_palette(builtin.name())
            .expect("builtin palette table covers every BuiltinColormap variant");
        Self::from_palette(palette, min, max)
    }

    /// Create a LUT colormap from a [`Palette`](crate::palette::Palette),
    /// honouring its interpolation mode and explicit nodata color.
    ///
    /// Note: a `Step` palette is sampled onto the 4096-entry LUT, so class
    /// thresholds land within LUT resolution — `(max - min) / 4095` — of
    /// their exact stop values.
    pub fn from_palette(palette: &crate::palette::Palette, min: f64, max: f64) -> Self {
        let mut lut = Self::from_stops_interp(&palette.stops, min, max, palette.interpolation);
        if let Some(nodata) = palette.nodata_color {
            lut.nodata_color = nodata;
        }
        lut
    }

    /// Pre-compute an integer LUT from this colormap over `[min_val, max_val]`.
    ///
    /// Returns `None` if the range exceeds 65,536 entries.
    pub fn to_integer_lut(&self, min_val: i64, max_val: i64) -> Option<IntegerLutColorMap> {
        IntegerLutColorMap::from_colormap(self, min_val, max_val)
    }

    /// Create a LUT colormap from color stops.
    ///
    /// The `min`/`max` define the data value range. The color stops define
    /// which colors appear at which values within (or beyond) that range.
    /// The LUT has 4096 entries for better precision with wide value ranges.
    pub fn from_stops(stops: &[ColorStop], min: f64, max: f64) -> Self {
        Self::from_stops_interp(stops, min, max, crate::palette::Interpolation::Linear)
    }

    /// Like [`from_stops`](Self::from_stops), with an explicit
    /// interpolation mode.
    pub fn from_stops_interp(
        stops: &[ColorStop],
        min: f64,
        max: f64,
        interp: crate::palette::Interpolation,
    ) -> Self {
        let lut_size = 4096;
        let mut lut = Vec::with_capacity(lut_size);

        for i in 0..lut_size {
            let t = i as f64 / (lut_size - 1) as f64;
            let value = min + t * (max - min);
            lut.push(sample_stops(stops, value, interp));
        }

        Self {
            lut,
            min,
            max,
            nodata_color: [0, 0, 0, 0],
            underflow_color: sample_stops(stops, min.next_down(), interp),
        }
    }
}

impl ColorMap for LutColorMap {
    fn nodata_color(&self) -> [u8; 4] {
        self.nodata_color
    }

    fn color(&self, value: Option<f64>) -> [u8; 4] {
        match value {
            None => self.nodata_color,
            // NaN/±∞ are not real data — colour them as nodata. Deliberate
            // behaviour change: before #250, NaN here hit the saturating cast
            // `NaN as usize = 0` and returned `lut[0]` (opaque for built-ins
            // like Viridis/Temperature); `LinearColorMap` returned the LAST
            // stop. The two were quietly inconsistent. This guard unifies all
            // three colormap impls on `nodata_color`, which is what a NaN
            // pixel actually means.
            Some(v) if !v.is_finite() => self.nodata_color,
            // Strictly below the domain: the probed underflow color (clamp
            // color for ordinary palettes, transparent past a display-
            // threshold guard).
            Some(v) if v < self.min => self.underflow_color,
            Some(v) => {
                if self.max <= self.min {
                    return self.lut[0];
                }
                let t = (v - self.min) / (self.max - self.min);
                let idx = (t * (self.lut.len() - 1) as f64)
                    .round()
                    .clamp(0.0, (self.lut.len() - 1) as f64) as usize;
                self.lut[idx]
            }
        }
    }
}

/// Pre-computed integer LUT colormap. O(1) per pixel with direct indexing.
///
/// All colors over the configured integer range are pre-computed into a flat
/// array. Per-pixel lookup is one subtract + one `.round()` + clamp + index —
/// matching [`LutColorMap`]'s round-nearest + saturate-clamp semantics at
/// integer entries, while replacing its `(v - min) / (max - min) * (len - 1)`
/// normalisation with a single offset shift. Values strictly below the
/// covered range return the source's color just under the range (the
/// underflow probe — clamp color for ordinary sources, transparent past a
/// display-threshold guard); values above saturate to the last entry.
///
/// Maximum supported range: 65,536 entries (~256 KB for UInt16/Int16).
pub struct IntegerLutColorMap {
    lut: Vec<[u8; 4]>,
    offset: i64,
    nodata_color: [u8; 4],
    /// Color for values strictly below the covered range — probed from the
    /// source just under `min_val`, mirroring [`LutColorMap`]'s underflow
    /// semantics (clamp color normally, transparent past a guard).
    underflow_color: [u8; 4],
}

/// Maximum number of entries in an integer LUT (covers full UInt16 / Int16 range).
const MAX_INTEGER_LUT_SIZE: usize = 65_536;

impl IntegerLutColorMap {
    /// Pre-compute a LUT for integer data in the range `[min_val, max_val]` (inclusive).
    ///
    /// Each integer value in the range gets its color computed once from the
    /// source colormap. At render time, lookup is `O(1)` per pixel.
    ///
    /// Returns `None` if the range exceeds 65,536 entries.
    pub fn from_colormap(source: &dyn ColorMap, min_val: i64, max_val: i64) -> Option<Self> {
        let nodata_color = source.nodata_color();
        let underflow_color = source.color(Some((min_val as f64).next_down()));
        if max_val < min_val {
            return Some(Self {
                lut: Vec::new(),
                offset: 0,
                nodata_color,
                underflow_color,
            });
        }
        let range = (max_val - min_val + 1) as usize;
        if range > MAX_INTEGER_LUT_SIZE {
            return None;
        }
        let mut lut = Vec::with_capacity(range);
        for i in 0..range {
            let value = (i as i64 + min_val) as f64;
            lut.push(source.color(Some(value)));
        }
        Some(Self {
            lut,
            offset: min_val,
            nodata_color,
            underflow_color,
        })
    }
}

impl ColorMap for IntegerLutColorMap {
    fn nodata_color(&self) -> [u8; 4] {
        self.nodata_color
    }

    fn color(&self, value: Option<f64>) -> [u8; 4] {
        // Round-nearest + saturate-clamp, matching `LutColorMap`/`LinearColorMap`
        // semantics. `v as i64` would truncate toward zero (23.6 → 23, -0.7 → 0)
        // which diverges from the float path's `.round()` (24 and -1). Out-of-range
        // values clamp to the first/last entry rather than fall to transparent,
        // also matching the float path; transparent here would silently hide
        // pixels just outside the configured range.
        match value {
            None => self.nodata_color,
            Some(v) if !v.is_finite() => self.nodata_color,
            Some(v) => {
                if self.lut.is_empty() {
                    return self.nodata_color;
                }
                // Strictly below the covered range: the probed underflow
                // color, BEFORE round-to-nearest — a display threshold at
                // 10 must hide 9.7, not round it up into visibility.
                if v < self.offset as f64 {
                    return self.underflow_color;
                }
                let last = (self.lut.len() - 1) as i64;
                let idx = (v - self.offset as f64).round() as i64;
                self.lut[idx.clamp(0, last) as usize]
            }
        }
    }
}

/// Wraps an inner colormap so a single reserved **sentinel** value renders as
/// one fixed colour, while every other value (and nodata) delegates to the
/// inner colormap.
///
/// This is how a single colormap-driven raster layer carries two visually
/// distinct symbol classes: the derived storm-cell (`CELLS`) overlay paints
/// cell outlines/markers at their dBZ value (→ the inner radar ramp) and
/// track trails at the sentinel (→ one neutral colour), so trails read as
/// subordinate to the intensity-coloured cells instead of blending into them.
/// The sentinel is matched by exact `f64` equality, so the producer must
/// paint trails at the identical constant (`==`, no rounding).
pub struct OverlayColorMap {
    inner: std::sync::Arc<dyn ColorMap>,
    sentinel: f64,
    overlay_color: [u8; 4],
}

impl OverlayColorMap {
    pub fn new(inner: std::sync::Arc<dyn ColorMap>, sentinel: f64, overlay_color: [u8; 4]) -> Self {
        // A NaN sentinel could never match (`NaN != NaN`), silently making this
        // a pure passthrough — catch that misuse in debug builds.
        debug_assert!(
            sentinel.is_finite(),
            "OverlayColorMap sentinel must be finite; NaN/±inf never match"
        );
        Self {
            inner,
            sentinel,
            overlay_color,
        }
    }
}

impl ColorMap for OverlayColorMap {
    fn nodata_color(&self) -> [u8; 4] {
        self.inner.nodata_color()
    }

    fn color(&self, value: Option<f64>) -> [u8; 4] {
        match value {
            // Exact match (the producer paints this literal constant), and
            // finite so it never collides with the nodata/NaN path.
            Some(v) if v == self.sentinel => self.overlay_color,
            other => self.inner.color(other),
        }
    }
}

/// Linear interpolation colormap for continuous data.
pub struct LinearColorMap {
    stops: Vec<ColorStop>,
    nodata_color: [u8; 4],
}

impl LinearColorMap {
    pub fn new(stops: Vec<ColorStop>) -> Self {
        Self {
            stops,
            nodata_color: [0, 0, 0, 0],
        }
    }

    /// Pre-compute an integer LUT for this colormap over `[min_val, max_val]`.
    ///
    /// Returns `None` if the range exceeds 65,536 entries.
    pub fn to_integer_lut(&self, min_val: i64, max_val: i64) -> Option<IntegerLutColorMap> {
        IntegerLutColorMap::from_colormap(self, min_val, max_val)
    }
}

impl ColorMap for LinearColorMap {
    fn nodata_color(&self) -> [u8; 4] {
        self.nodata_color
    }

    fn color(&self, value: Option<f64>) -> [u8; 4] {
        match value {
            None => self.nodata_color,
            // Match the NaN guard in LutColorMap / IntegerLutColorMap — otherwise
            // a NaN falls through every comparison in `interpolate_stops` and
            // returns the LAST stop's colour, diverging from the other paths.
            Some(v) if !v.is_finite() => self.nodata_color,
            Some(v) => interpolate_stops(&self.stops, v),
        }
    }
}

/// Sample color stops at a value with the given interpolation mode.
pub(crate) fn sample_stops(
    stops: &[ColorStop],
    value: f64,
    interp: crate::palette::Interpolation,
) -> [u8; 4] {
    match interp {
        crate::palette::Interpolation::Linear => interpolate_stops(stops, value),
        crate::palette::Interpolation::Step => step_stops(stops, value),
    }
}

/// Discrete-class sampling: the color of the highest stop at or below
/// `value`. Below the first stop clamps to the first stop's color, matching
/// [`interpolate_stops`]' clamp semantics; empty stops likewise match its
/// black fallback.
fn step_stops(stops: &[ColorStop], value: f64) -> [u8; 4] {
    if stops.is_empty() {
        return [0, 0, 0, 255];
    }
    let mut color = stops[0].color;
    for stop in stops {
        if stop.value <= value {
            color = stop.color;
        } else {
            break;
        }
    }
    color
}

/// Interpolate between color stops to find the color for a given value.
fn interpolate_stops(stops: &[ColorStop], value: f64) -> [u8; 4] {
    if stops.is_empty() {
        return [0, 0, 0, 255];
    }
    if stops.len() == 1 || value <= stops[0].value {
        return stops[0].color;
    }
    if value >= stops[stops.len() - 1].value {
        return stops[stops.len() - 1].color;
    }

    // Find the two surrounding stops
    for i in 0..stops.len() - 1 {
        let lo = &stops[i];
        let hi = &stops[i + 1];
        if value >= lo.value && value <= hi.value {
            let range = hi.value - lo.value;
            if range == 0.0 {
                return lo.color;
            }
            let t = (value - lo.value) / range;
            return [
                lerp_u8(lo.color[0], hi.color[0], t),
                lerp_u8(lo.color[1], hi.color[1], t),
                lerp_u8(lo.color[2], hi.color[2], t),
                lerp_u8(lo.color[3], hi.color[3], t),
            ];
        }
    }

    stops[stops.len() - 1].color
}

fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t).round() as u8
}

/// Parse a hex color string like "#RRGGBB" or "#RRGGBBAA".
pub fn parse_hex_color(s: &str) -> Result<[u8; 4], String> {
    let s = s.strip_prefix('#').unwrap_or(s);
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).map_err(|e| e.to_string())?;
            let g = u8::from_str_radix(&s[2..4], 16).map_err(|e| e.to_string())?;
            let b = u8::from_str_radix(&s[4..6], 16).map_err(|e| e.to_string())?;
            Ok([r, g, b, 255])
        }
        8 => {
            let r = u8::from_str_radix(&s[0..2], 16).map_err(|e| e.to_string())?;
            let g = u8::from_str_radix(&s[2..4], 16).map_err(|e| e.to_string())?;
            let b = u8::from_str_radix(&s[4..6], 16).map_err(|e| e.to_string())?;
            let a = u8::from_str_radix(&s[6..8], 16).map_err(|e| e.to_string())?;
            Ok([r, g, b, a])
        }
        _ => Err(format!(
            "invalid hex color: '{s}' (expected 6 or 8 hex digits)"
        )),
    }
}

/// Get built-in color stops for a named colormap.
///
/// Legacy shim over the [`crate::palette`] builtin table (the single
/// source of palette data).
pub fn builtin_stops(builtin: &BuiltinColormap) -> Vec<ColorStop> {
    crate::palette::builtin_palette(builtin.name())
        .expect("builtin palette table covers every BuiltinColormap variant")
        .stops
        .clone()
}

/// Resolve a built-in colormap name to its enum variant.
///
/// Legacy shim: only covers the original 12 palettes that have enum
/// variants. Prefer [`crate::palette::builtin_palette`], which also
/// resolves the newer builtins (`radial_velocity`, `pressure`, `humidity`,
/// `cloud_cover`).
pub fn resolve_builtin(name: &str) -> Option<BuiltinColormap> {
    match name {
        "radar_dbz" => Some(BuiltinColormap::RadarDbz),
        "radar_smhi" => Some(BuiltinColormap::RadarSmhi),
        "radar_fmi" => Some(BuiltinColormap::RadarFmi),
        "radar_bookbinder" => Some(BuiltinColormap::RadarBookbinder),
        "grayscale" => Some(BuiltinColormap::Grayscale),
        "viridis" => Some(BuiltinColormap::Viridis),
        "temperature" => Some(BuiltinColormap::Temperature),
        "precipitation" => Some(BuiltinColormap::Precipitation),
        "precipitation_rate" => Some(BuiltinColormap::PrecipitationRate),
        "wind_speed" => Some(BuiltinColormap::WindSpeed),
        "cap_severity" => Some(BuiltinColormap::CapSeverity),
        "lightning_age" => Some(BuiltinColormap::LightningAge),
        _ => None,
    }
}

/// List all available built-in colormap names (including the newer
/// palettes that have no `BuiltinColormap` variant).
pub fn builtin_names() -> &'static [&'static str] {
    crate::palette::builtin_names()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lut_colormap_boundaries() {
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 100.0);
        let black = cmap.color(Some(0.0));
        let white = cmap.color(Some(100.0));
        assert_eq!(black, [0, 0, 0, 255]);
        assert_eq!(white, [255, 255, 255, 255]);
    }

    #[test]
    fn test_lut_colormap_nodata() {
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 100.0);
        assert_eq!(cmap.color(None), [0, 0, 0, 0]);
    }

    #[test]
    fn test_overlay_colormap_sentinel_and_passthrough() {
        let inner = std::sync::Arc::new(LutColorMap::from_builtin(
            BuiltinColormap::Grayscale,
            0.0,
            100.0,
        ));
        let overlay = OverlayColorMap::new(inner, -9999.0, [60, 60, 60, 255]);
        // The sentinel renders as the fixed overlay colour…
        assert_eq!(overlay.color(Some(-9999.0)), [60, 60, 60, 255]);
        // …every other value delegates to the inner colormap…
        assert_eq!(overlay.color(Some(0.0)), [0, 0, 0, 255]);
        assert_eq!(overlay.color(Some(100.0)), [255, 255, 255, 255]);
        // …and nodata / NaN follow the inner colormap's nodata colour, never
        // the overlay colour (sentinel match is finite-exact).
        assert_eq!(overlay.color(None), [0, 0, 0, 0]);
        assert_eq!(overlay.color(Some(f64::NAN)), [0, 0, 0, 0]);
        // A near-but-not-equal value is NOT the sentinel: it delegates to the
        // inner colormap (here clamped to the min/black end), never the
        // overlay colour.
        let inner = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 100.0);
        assert_eq!(overlay.color(Some(-9998.0)), inner.color(Some(-9998.0)));
        assert_ne!(overlay.color(Some(-9998.0)), [60, 60, 60, 255]);
    }

    #[test]
    fn test_lut_colormap_clamp() {
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 100.0);
        // Below min should clamp to first color
        assert_eq!(cmap.color(Some(-10.0)), [0, 0, 0, 255]);
        // Above max should clamp to last color
        assert_eq!(cmap.color(Some(200.0)), [255, 255, 255, 255]);
    }

    /// Pin the enum↔table invariant: EVERY `BuiltinColormap` variant has a
    /// non-empty row in the palette table (`builtin_stops` would panic
    /// otherwise) and its `name()` resolves back to the variant through
    /// `resolve_builtin`. A variant added without a table row — or a rename
    /// on either side — fails here instead of panicking at style-build time.
    #[test]
    fn every_builtin_variant_round_trips_through_the_palette_table() {
        let variants = [
            BuiltinColormap::RadarDbz,
            BuiltinColormap::RadarSmhi,
            BuiltinColormap::RadarFmi,
            BuiltinColormap::RadarBookbinder,
            BuiltinColormap::Grayscale,
            BuiltinColormap::Viridis,
            BuiltinColormap::Temperature,
            BuiltinColormap::Precipitation,
            BuiltinColormap::PrecipitationRate,
            BuiltinColormap::WindSpeed,
            BuiltinColormap::CapSeverity,
            BuiltinColormap::LightningAge,
        ];
        assert_eq!(variants.len(), 12, "the legacy shim covers 12 palettes");
        for v in &variants {
            let stops = builtin_stops(v);
            assert!(!stops.is_empty(), "{} has no stops in the table", v.name());
            let resolved = resolve_builtin(v.name())
                .unwrap_or_else(|| panic!("{} does not resolve back to a variant", v.name()));
            assert_eq!(
                resolved.name(),
                v.name(),
                "resolve_builtin must map the name back to the same variant"
            );
        }
    }

    #[test]
    fn test_cap_severity_ramp_resolves_and_colours_codes() {
        let builtin = resolve_builtin("cap_severity").expect("cap_severity is a builtin");
        // Severity codes 0..4 sit exactly on the stops, so a 0..4 LUT colours
        // each code with its stop colour (no interpolation between codes).
        let cmap = LutColorMap::from_builtin(builtin, 0.0, 4.0);
        assert_eq!(cmap.color(Some(0.0)), [120, 120, 120, 200]); // Unknown
        assert_eq!(cmap.color(Some(1.0)), [38, 166, 91, 200]); // Minor
        assert_eq!(cmap.color(Some(2.0)), [241, 196, 15, 200]); // Moderate
        assert_eq!(cmap.color(Some(3.0)), [230, 126, 34, 200]); // Severe
        assert_eq!(cmap.color(Some(4.0)), [192, 57, 43, 200]); // Extreme
                                                               // No alert (nodata) stays fully transparent.
        assert_eq!(cmap.color(None), [0, 0, 0, 0]);
        assert!(builtin_names().contains(&"cap_severity"));
    }

    #[test]
    fn test_parse_hex_color() {
        assert_eq!(parse_hex_color("#FF0000"), Ok([255, 0, 0, 255]));
        assert_eq!(parse_hex_color("#00FF0080"), Ok([0, 255, 0, 128]));
        assert_eq!(parse_hex_color("AABBCC"), Ok([170, 187, 204, 255]));
        assert!(parse_hex_color("#FFF").is_err());
    }

    #[test]
    fn test_linear_colormap() {
        let cmap = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 100.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let mid = cmap.color(Some(50.0));
        assert_eq!(mid, [128, 128, 128, 255]);
    }

    // --- IntegerLutColorMap tests ---

    #[test]
    fn test_integer_lut_matches_linear_colormap() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 255.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = linear.to_integer_lut(0, 255).unwrap();

        // Every integer value should produce the same color
        for i in 0..=255 {
            assert_eq!(
                lut.color(Some(i as f64)),
                linear.color(Some(i as f64)),
                "mismatch at value {i}"
            );
        }
    }

    #[test]
    fn test_integer_lut_signed_offset() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: -128.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 127.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, -128, 127).unwrap();

        // Check boundaries
        assert_eq!(lut.color(Some(-128.0)), linear.color(Some(-128.0)));
        assert_eq!(lut.color(Some(0.0)), linear.color(Some(0.0)));
        assert_eq!(lut.color(Some(127.0)), linear.color(Some(127.0)));

        // Check all values match
        for i in -128..=127_i64 {
            assert_eq!(
                lut.color(Some(i as f64)),
                linear.color(Some(i as f64)),
                "mismatch at value {i}"
            );
        }
    }

    #[test]
    fn test_integer_lut_out_of_range_saturates() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [100, 100, 100, 255],
            },
            ColorStop {
                value: 10.0,
                color: [200, 200, 200, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, 0, 10).unwrap();

        // Out of range below → first entry (saturate, matching LutColorMap clamp).
        assert_eq!(lut.color(Some(-1.0)), linear.color(Some(0.0)));
        assert_eq!(lut.color(Some(-100.0)), linear.color(Some(0.0)));
        // Out of range above → last entry.
        assert_eq!(lut.color(Some(11.0)), linear.color(Some(10.0)));
        assert_eq!(lut.color(Some(1e9)), linear.color(Some(10.0)));
    }

    #[test]
    fn test_all_colormaps_treat_nan_and_infinity_as_nodata() {
        // All three colormap types must agree on non-finite inputs — otherwise
        // `maybe_wrap_integer_lut` (the #207 wrap) silently changes behaviour
        // when an engine lets NaN reach the colorizer.
        let stops = vec![
            ColorStop {
                value: 0.0,
                color: [255, 0, 0, 255],
            },
            ColorStop {
                value: 10.0,
                color: [0, 255, 0, 255],
            },
        ];
        let lut = LutColorMap::from_stops(&stops, 0.0, 10.0);
        let lin = LinearColorMap::new(stops);
        let int_lut = IntegerLutColorMap::from_colormap(&lin, 0, 10).unwrap();
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(lut.color(Some(v)), [0, 0, 0, 0], "LutColorMap on {v}");
            assert_eq!(lin.color(Some(v)), [0, 0, 0, 0], "LinearColorMap on {v}");
            assert_eq!(
                int_lut.color(Some(v)),
                [0, 0, 0, 0],
                "IntegerLutColorMap on {v}"
            );
        }
    }

    #[test]
    fn test_integer_lut_rounds_nearest_not_toward_zero() {
        // Pins round-nearest in IntegerLutColorMap (not truncate-toward-zero,
        // which would diverge for negative values). We pin behaviour INTERNAL
        // to the LUT here — IntegerLutColorMap has 1 entry per integer unit
        // while LutColorMap uses a fixed 4096-entry table, so the two paths
        // can't be expected to match at non-integer inputs.
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: -10.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 10.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, -10, 10).unwrap();
        // 4.7 rounds up to 5, NOT toward zero (which would yield 4).
        assert_eq!(lut.color(Some(4.7)), lut.color(Some(5.0)));
        assert_ne!(lut.color(Some(4.7)), lut.color(Some(4.0)));
        // -0.7 rounds to the nearer integer (-1), NOT toward zero (which would
        // collapse it onto entry 0 — the bug the rounding fix prevents).
        assert_ne!(lut.color(Some(-0.7)), lut.color(Some(0.0)));
        assert_eq!(lut.color(Some(-0.7)), lut.color(Some(-1.0)));
        // -0.5 is half-way; Rust's `.round()` rounds away from zero in the
        // (non-negative) index-space coordinate after the offset shift, so in
        // value space it rounds toward `max` here.
        assert_eq!(lut.color(Some(-0.5)), lut.color(Some(0.0)));
    }

    #[test]
    fn test_integer_lut_nodata() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [255, 0, 0, 255],
            },
            ColorStop {
                value: 10.0,
                color: [0, 255, 0, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, 0, 10).unwrap();
        assert_eq!(lut.color(None), [0, 0, 0, 0]);
    }

    /// `from_colormap` must copy the source's `nodata_color`, not hardcode
    /// transparent — otherwise a colormap with grey/opaque nodata would
    /// silently flip to transparent when wrapped.
    #[test]
    fn test_integer_lut_preserves_source_nodata_color() {
        // Mock colormap with a non-default (opaque grey) nodata colour.
        struct GreyNodata;
        impl ColorMap for GreyNodata {
            fn color(&self, value: Option<f64>) -> [u8; 4] {
                match value {
                    None => self.nodata_color(),
                    Some(v) if !v.is_finite() => self.nodata_color(),
                    Some(_) => [255, 0, 0, 255],
                }
            }
            fn nodata_color(&self) -> [u8; 4] {
                [128, 128, 128, 255]
            }
        }

        let lut = IntegerLutColorMap::from_colormap(&GreyNodata, 0, 10).unwrap();
        assert_eq!(lut.color(None), [128, 128, 128, 255]);
        assert_eq!(lut.color(Some(f64::NAN)), [128, 128, 128, 255]);
        assert_eq!(lut.nodata_color(), [128, 128, 128, 255]);

        // Empty-range path (max < min) must propagate it too.
        let empty = IntegerLutColorMap::from_colormap(&GreyNodata, 10, 5).unwrap();
        assert_eq!(empty.color(None), [128, 128, 128, 255]);
        assert_eq!(empty.color(Some(0.0)), [128, 128, 128, 255]);
    }

    #[test]
    fn test_integer_lut_single_value_range() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 100.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, 50, 50).unwrap();
        assert_eq!(lut.color(Some(50.0)), linear.color(Some(50.0)));
        // Below range → the source's color just under the range (underflow
        // probe; equals the clamp color whenever the source is constant
        // below its domain, which every real render path is). Above range
        // saturates to the last entry as before.
        assert_eq!(lut.color(Some(49.0)), linear.color(Some(49.999)));
        assert_eq!(lut.color(Some(51.0)), linear.color(Some(50.0)));
    }

    #[test]
    fn test_integer_lut_full_uint16_range() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 65535.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, 0, 65535).unwrap();

        // Check boundaries
        assert_eq!(lut.color(Some(0.0)), [0, 0, 0, 255]);
        assert_eq!(lut.color(Some(65535.0)), [255, 255, 255, 255]);
        // Check midpoint
        assert_eq!(lut.color(Some(32768.0)), linear.color(Some(32768.0)));
    }

    #[test]
    fn test_integer_lut_full_int16_range() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: -32768.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 32767.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, -32768, 32767).unwrap();

        assert_eq!(lut.color(Some(-32768.0)), [0, 0, 0, 255]);
        assert_eq!(lut.color(Some(32767.0)), [255, 255, 255, 255]);
        assert_eq!(lut.color(Some(0.0)), linear.color(Some(0.0)));
    }

    #[test]
    fn test_integer_lut_rejects_too_large_range() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 1.0,
                color: [255, 255, 255, 255],
            },
        ]);
        // 65537 entries — one too many
        assert!(IntegerLutColorMap::from_colormap(&linear, 0, 65536).is_none());
    }

    #[test]
    fn test_integer_lut_empty_range() {
        let linear = LinearColorMap::new(vec![ColorStop {
            value: 0.0,
            color: [255, 0, 0, 255],
        }]);
        // min > max → empty
        let lut = IntegerLutColorMap::from_colormap(&linear, 10, 5).unwrap();
        assert_eq!(lut.color(Some(7.0)), [0, 0, 0, 0]);
    }

    #[test]
    fn test_integer_lut_from_builtin_lut() {
        let builtin = LutColorMap::from_builtin(BuiltinColormap::RadarDbz, 0.0, 70.0);
        let int_lut = builtin.to_integer_lut(0, 70).unwrap();

        // Verify key values match
        for v in [0, 5, 10, 25, 35, 50, 70] {
            assert_eq!(
                int_lut.color(Some(v as f64)),
                builtin.color(Some(v as f64)),
                "mismatch at dBZ {v}"
            );
        }
    }
}
