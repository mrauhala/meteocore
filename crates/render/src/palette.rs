//! Named palette model — the single source of truth for built-in colormaps
//! and the registry that user-defined palettes load into.
//!
//! A [`Palette`] is a named list of color stops plus an interpolation mode.
//! Built-ins are rows in one static table ([`builtin_palettes`]); user
//! palettes (from `[[colormaps]]` config or a colormaps directory) are
//! inserted into a [`PaletteRegistry`] at config load. Everything that
//! resolves a colormap by name goes through that registry (or
//! [`builtin_palette`] for builtins only), so there is exactly one list to
//! maintain.
//!
//! The legacy `BuiltinColormap` enum in [`crate::colormap`] is a shim over
//! this table, kept for existing `LutColorMap::from_builtin` callers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use crate::colormap::{mirror, sample_stops, ColorStop};

/// How colors are produced between stops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Interpolation {
    /// Linear RGBA interpolation between bracketing stops (the historical
    /// behavior).
    #[default]
    Linear,
    /// Discrete classes: a value takes the color of the highest stop at or
    /// below it. Values below the first stop clamp to the first stop's
    /// color, matching the linear mode's clamp semantics.
    Step,
}

/// A named, reusable color palette.
///
/// Stops are kept sorted ascending by value (enforced by [`Palette::new`]).
#[derive(Clone, Debug)]
pub struct Palette {
    /// Registry key, referenced by `colormap = "<name>"` in config.
    pub name: String,
    /// Human-readable title (legend/UI).
    pub title: Option<String>,
    /// Sorted ascending by value.
    pub stops: Vec<ColorStop>,
    pub interpolation: Interpolation,
    /// True when the stops span a normalized 0..1 domain (viridis,
    /// grayscale) rather than physical values. Such palettes need an
    /// explicit or derived value range to render meaningfully —
    /// [`Palette::default_range`] returns `None` for them.
    pub normalized: bool,
    /// Explicit nodata color (e.g. from a `.cpt` `N` line). `None` = the
    /// colormap default (fully transparent).
    pub nodata_color: Option<[u8; 4]>,
}

impl Palette {
    /// Create a palette, sorting the stops ascending by value.
    pub fn new(
        name: impl Into<String>,
        stops: Vec<ColorStop>,
        interpolation: Interpolation,
    ) -> Self {
        let mut stops = stops;
        stops.sort_by(|a, b| a.value.total_cmp(&b.value));
        Self {
            name: name.into(),
            title: None,
            stops,
            interpolation,
            normalized: false,
            nodata_color: None,
        }
    }

    /// The value range implied by the stops (first..last), or `None` when
    /// the palette is [`normalized`](Self::normalized) or has fewer than two
    /// distinct stop values — callers must then supply a range themselves.
    pub fn default_range(&self) -> Option<(f64, f64)> {
        if self.normalized {
            return None;
        }
        let first = self.domain_min_stop();
        match (first, self.stops.last()) {
            (Some(first), Some(last)) if last.value > first.value => {
                Some((first.value, last.value))
            }
            _ => None,
        }
    }

    /// A [`normalized`](Self::normalized) palette's stops stretched from
    /// their 0..1 domain onto the physical range `[min, max]` (#823), so it
    /// is sampled and legended in data units; reversed over it when
    /// `min > max` ([`Self::reversed_over`]). A data-valued palette comes
    /// back unchanged.
    pub fn stretched(&self, min: f64, max: f64) -> Palette {
        if !self.normalized {
            return self.clone();
        }
        let (lo, hi) = ascending(min, max);
        let stretched = self.remapped(0.0, 1.0, lo, hi);
        if max < min {
            stretched.reversed_over(lo, hi)
        } else {
            stretched
        }
    }

    /// The palette to sample over a style range `[min, max]`, when that is
    /// not this palette as it stands (#823). `None` means sample it as is.
    ///
    /// - A [`normalized`](Self::normalized) palette always spans the range.
    /// - A data-valued palette keeps its physical values: a range that
    ///   overlaps its stops clips, so 0 °C keeps its colour. Only a range
    ///   lying entirely outside its stops is rescaled onto them, because
    ///   clipping could then paint only one colour. That is a unit mismatch,
    ///   such as the °C `temperature` palette given a kelvin range.
    /// - A range given high to low (`min > max`) is fitted as the same range
    ///   low to high, by the rules above, then [reversed](Self::reversed_over)
    ///   over it (#872). That is the palette a [`LutColorMap`] over
    ///   `min..max` draws and the legend shows.
    ///
    /// [`LutColorMap`]: crate::colormap::LutColorMap
    pub fn fitted_to(&self, min: f64, max: f64) -> Option<Palette> {
        let (lo, hi) = ascending(min, max);
        let fitted = self.fitted_ascending(lo, hi);
        if max < min {
            return Some(fitted.as_ref().unwrap_or(self).reversed_over(lo, hi));
        }
        fitted
    }

    /// [`Self::fitted_to`] for a range given low to high.
    fn fitted_ascending(&self, min: f64, max: f64) -> Option<Palette> {
        if self.normalized {
            return Some(self.remapped(0.0, 1.0, min, max));
        }
        let domain = self.domain_stops();
        let (lo, hi) = (domain.first()?.value, domain.last()?.value);
        let outside = max < lo || min > hi;
        (outside && hi > lo && min.is_finite() && max.is_finite())
            .then(|| self.remapped(lo, hi, min, max))
    }

    /// Every stop mapped linearly from `[from_lo, from_hi]` onto
    /// `[min, max]` (`min <= max`), as a data-valued palette.
    fn remapped(&self, from_lo: f64, from_hi: f64, min: f64, max: f64) -> Palette {
        let scale = (max - min) / (from_hi - from_lo);
        let mut stops: Vec<ColorStop> = self
            .stops
            .iter()
            .map(|s| ColorStop {
                // The domain's ends land exactly on the range's.
                value: if s.value == from_hi {
                    max
                } else {
                    min + (s.value - from_lo) * scale
                },
                color: s.color,
            })
            .collect();
        // Keep a display-threshold guard exactly one ULP outside its stop,
        // which is what makes it a guard ([`Self::domain_stops`]).
        let n = stops.len();
        if leading_guard(&self.stops) {
            stops[0].value = stops[1].value.next_down();
        }
        if trailing_guard(&self.stops) {
            stops[n - 1].value = stops[n - 2].value.next_up();
        }
        Palette {
            stops,
            normalized: false,
            ..self.clone()
        }
    }

    /// This palette reversed over `[lo, hi]` (#872): each value takes the
    /// colour of its mirror image `lo + hi - v`, which is how a style range
    /// given high to low draws its palette. Stops stay ascending, which the
    /// samplers and legends assume.
    ///
    /// - Linear: every stop moves to its mirror image. A hard edge's paired
    ///   stops swap, keeping each colour on its side, and a display-threshold
    ///   guard follows its stop to the other end, one ULP past it, so the
    ///   values beyond the threshold still vanish ([`Self::domain_stops`]).
    /// - Step: a class keeps its colour over its mirrored bounds, so each
    ///   colour moves to the stop that closed its class; the class below the
    ///   first boundary starts at `lo`. A value exactly on a boundary falls
    ///   in the class above it here, while a [`LutColorMap`] over the
    ///   reversed range keeps it in the class of its mirror image.
    ///
    /// [`LutColorMap`]: crate::colormap::LutColorMap
    pub fn reversed_over(&self, lo: f64, hi: f64) -> Palette {
        let at = |s: &ColorStop| mirror(s.value, lo, hi);
        let mut stops: Vec<ColorStop> = match self.interpolation {
            Interpolation::Linear => {
                let mut stops: Vec<ColorStop> = self
                    .stops
                    .iter()
                    .rev()
                    .map(|s| ColorStop {
                        value: at(s),
                        color: s.color,
                    })
                    .collect();
                let n = stops.len();
                if leading_guard(&self.stops) {
                    stops[n - 1].value = stops[n - 2].value.next_up();
                }
                if trailing_guard(&self.stops) {
                    stops[0].value = stops[1].value.next_down();
                }
                stops
            }
            // Class i, `[s_i, s_{i+1})` in colour c_i, mirrors onto
            // `(m(s_{i+1}), m(s_i)]`: c_i starts at m(s_{i+1}). The top class,
            // from s_n up, ends at m(s_n) and starts at `lo`.
            Interpolation::Step => match self.stops.last() {
                None => Vec::new(),
                Some(top) => std::iter::once(ColorStop {
                    value: lo.min(at(top)),
                    color: top.color,
                })
                .chain(self.stops.windows(2).rev().map(|w| ColorStop {
                    value: at(&w[1]),
                    color: w[0].color,
                }))
                .collect(),
            },
        };
        // The mirror is monotone, but exact at the ends only: a stable sort
        // undoes any one-ULP inversion next to them, keeping equal stops in
        // their order.
        stops.sort_by(|a, b| a.value.total_cmp(&b.value));
        Palette {
            stops,
            ..self.clone()
        }
    }

    /// Sample the palette color at a physical value (no range scaling).
    pub fn sample(&self, value: f64) -> [u8; 4] {
        sample_stops(&self.stops, value, self.interpolation)
    }

    /// The stop that defines the domain minimum. Normally the first stop —
    /// EXCEPT when it is a display-threshold guard: a fully-transparent
    /// stop sitting exactly one ULP below the following stop (the `.pal`
    /// importer emits these so below-minimum values vanish). A guard is
    /// not the domain edge; skipping it keeps derived ranges and legend
    /// minima at the real threshold (10, not 9.999…). Wide intentional
    /// transparent segments (e.g. radar_dbz 0→5 dBZ) are NOT skipped —
    /// the discriminator is the exact one-ULP placement.
    pub fn domain_min_stop(&self) -> Option<&ColorStop> {
        self.domain_stops().first()
    }

    /// The stops that define the palette's domain: all of them, minus a
    /// leading display-threshold guard (see [`domain_min_stop`]) and a
    /// trailing one, a fully-transparent last stop exactly one ULP above
    /// the stop before it, where [`Self::reversed_over`] moves a guard.
    /// Legend serialization uses this so the emitted stops start at the
    /// same value the derived `min` reports.
    pub fn domain_stops(&self) -> &[ColorStop] {
        let mut stops = &self.stops[..];
        if leading_guard(stops) {
            stops = &stops[1..];
        }
        if trailing_guard(stops) {
            stops = &stops[..stops.len() - 1];
        }
        stops
    }

    /// Hex-encode a stop color for machine-readable legends: `#RRGGBB`,
    /// or `#RRGGBBAA` when the alpha channel is not fully opaque.
    pub fn color_hex(color: [u8; 4]) -> String {
        if color[3] == 255 {
            format!("#{:02X}{:02X}{:02X}", color[0], color[1], color[2])
        } else {
            format!(
                "#{:02X}{:02X}{:02X}{:02X}",
                color[0], color[1], color[2], color[3]
            )
        }
    }
}

/// `[min, max]` low to high: a range given high to low swapped.
pub(crate) fn ascending(min: f64, max: f64) -> (f64, f64) {
    if max < min {
        (max, min)
    } else {
        (min, max)
    }
}

/// Whether `stops` open with a display-threshold guard: a fully-transparent
/// stop exactly one ULP below the next ([`Palette::domain_min_stop`]).
fn leading_guard(stops: &[ColorStop]) -> bool {
    matches!(stops, [first, second, ..]
        if first.color[3] == 0 && first.value == second.value.next_down())
}

/// Whether `stops` close with a display-threshold guard, as
/// [`Palette::reversed_over`] leaves a leading one: a fully-transparent
/// stop exactly one ULP above the one before it.
fn trailing_guard(stops: &[ColorStop]) -> bool {
    matches!(stops, [.., before, last]
        if last.color[3] == 0 && last.value == before.value.next_up())
}

impl Interpolation {
    /// The config / legend-JSON string form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Interpolation::Linear => "linear",
            Interpolation::Step => "step",
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in palette table
// ---------------------------------------------------------------------------

/// One row of the static builtin table. Stop data lives here as plain
/// tuples so the whole palette is a single `static` — [`builtin_palettes`]
/// materializes `Palette` values from it once.
struct BuiltinDef {
    name: &'static str,
    title: &'static str,
    normalized: bool,
    stops: &'static [(f64, [u8; 4])],
}

/// THE list of built-in palettes. Adding a palette = adding one row here;
/// name lookup, the names list, and (for the legacy 12) the
/// `BuiltinColormap` shim all derive from this table.
static BUILTIN_DEFS: &[BuiltinDef] = &[
    BuiltinDef {
        name: "radar_dbz",
        title: "Radar reflectivity (dBZ)",
        normalized: false,
        stops: &[
            (0.0, [0, 0, 0, 0]),          // transparent (no echo)
            (5.0, [0, 0, 0, 0]),          // transparent (below threshold)
            (5.1, [0, 128, 255, 255]),    // light blue
            (15.0, [0, 200, 255, 255]),   // cyan
            (25.0, [0, 200, 0, 255]),     // green
            (30.0, [0, 255, 0, 255]),     // bright green
            (35.0, [255, 255, 0, 255]),   // yellow
            (40.0, [255, 200, 0, 255]),   // orange-yellow
            (45.0, [255, 128, 0, 255]),   // orange
            (50.0, [255, 0, 0, 255]),     // red
            (55.0, [200, 0, 0, 255]),     // dark red
            (60.0, [180, 0, 180, 255]),   // magenta
            (70.0, [255, 255, 255, 255]), // white (extreme)
        ],
    },
    BuiltinDef {
        name: "radar_smhi",
        title: "SMHI radar reflectivity",
        normalized: false,
        // Gray tones below 5 dBZ, then blue → green → yellow → orange →
        // red → magenta → cyan, with per-dBZ colors.
        stops: &[
            (-30.0, [0, 0, 0, 0]), // below range: transparent
            (-29.1, [0, 0, 0, 0]),
            (-29.0, [54, 54, 54, 255]), // gray ramp starts
            (-20.0, [63, 63, 63, 255]),
            (-10.0, [73, 73, 73, 255]),
            (-6.0, [87, 87, 87, 255]),
            (-1.0, [139, 139, 139, 255]),
            (0.0, [150, 150, 150, 255]),
            (4.0, [192, 192, 192, 255]),
            (5.0, [0, 50, 255, 255]), // blue ramp
            (8.0, [0, 110, 255, 255]),
            (11.0, [0, 170, 255, 255]),
            (12.0, [0, 128, 0, 255]), // green ramp
            (15.0, [0, 163, 0, 255]),
            (19.0, [0, 178, 0, 255]),
            (20.0, [10, 208, 10, 255]), // bright green
            (24.0, [10, 248, 10, 255]),
            (25.0, [255, 255, 15, 255]), // yellow ramp
            (29.0, [255, 220, 15, 255]),
            (30.0, [255, 200, 0, 255]), // orange ramp
            (34.0, [255, 120, 0, 255]),
            (35.0, [255, 35, 35, 255]), // red ramp
            (37.0, [255, 0, 0, 255]),
            (40.0, [195, 0, 0, 255]),
            (44.0, [115, 0, 0, 255]),
            (45.0, [175, 0, 175, 255]), // magenta ramp
            (50.0, [219, 0, 219, 255]),
            (54.0, [255, 0, 255, 255]),
            (55.0, [0, 255, 255, 255]), // cyan ramp
            (60.0, [64, 255, 255, 255]),
            (65.0, [128, 255, 255, 255]),
            (70.0, [192, 255, 255, 255]),
        ],
    },
    BuiltinDef {
        name: "radar_fmi",
        title: "FMI radar reflectivity",
        normalized: false,
        // FMI summer radar reflectivity. Converted from SLD raw values
        // to dBZ via: dBZ = raw * 0.5 - 32. Below 5 dBZ is transparent.
        stops: &[
            (-32.0, [0, 0, 0, 0]),       // below range: transparent
            (5.0, [0, 0, 0, 0]),         // raw 74: below threshold
            (8.0, [108, 235, 243, 255]), // raw 80: light cyan
            (12.0, [88, 199, 151, 255]), // raw 88: green
            (18.0, [64, 152, 87, 255]),  // raw 100: dark green
            (24.0, [241, 243, 90, 255]), // raw 112: yellow
            (30.0, [223, 196, 10, 255]), // raw 124: gold
            (34.0, [235, 149, 26, 255]), // raw 132: orange
            (40.0, [232, 86, 22, 255]),  // raw 144: red-orange
            (46.0, [206, 2, 2, 255]),    // raw 156: red
            (52.0, [131, 10, 70, 255]),  // raw 168: dark magenta
            (58.0, [250, 81, 165, 255]), // raw 180: pink
        ],
    },
    BuiltinDef {
        name: "radar_bookbinder",
        title: "Bookbinder radar reflectivity",
        normalized: false,
        // Bookbinder 8-bit Z curve (Evan Bookbinder, WFO SGF).
        // Converted from SLD raw values to dBZ via: dBZ = raw * 0.5 - 32.
        stops: &[
            (-32.0, [0, 0, 0, 0]),        // raw 0: transparent
            (-31.5, [96, 96, 96, 77]),    // raw 1: faint gray
            (4.5, [96, 96, 96, 77]),      // raw 73: gray
            (5.0, [32, 96, 128, 179]),    // raw 74: dark cyan
            (19.5, [48, 208, 255, 255]),  // raw 103: bright cyan
            (20.0, [0, 255, 0, 255]),     // raw 104: bright green
            (39.5, [0, 76, 0, 255]),      // raw 143: dark green
            (40.0, [255, 230, 0, 255]),   // raw 144: yellow
            (49.5, [255, 128, 0, 255]),   // raw 163: orange
            (50.0, [255, 0, 0, 255]),     // raw 164: red
            (59.5, [96, 0, 0, 255]),      // raw 183: dark red
            (60.0, [255, 255, 255, 255]), // raw 184: white
            (64.5, [255, 255, 255, 255]), // raw 193: white
            (65.0, [144, 48, 208, 255]),  // raw 194: purple
            (69.5, [144, 48, 208, 255]),  // raw 203: purple
            (70.0, [255, 32, 255, 255]),  // raw 204: magenta
            (74.5, [255, 32, 255, 255]),  // raw 213: magenta
            (75.0, [255, 0, 128, 255]),   // raw 214: hot pink
            (79.5, [255, 0, 128, 255]),   // raw 223: hot pink
            (80.0, [255, 0, 150, 255]),   // raw 224: deep pink
            (94.5, [255, 0, 150, 255]),   // raw 253: deep pink
        ],
    },
    BuiltinDef {
        name: "grayscale",
        title: "Grayscale",
        normalized: true,
        stops: &[(0.0, [0, 0, 0, 255]), (1.0, [255, 255, 255, 255])],
    },
    BuiltinDef {
        name: "viridis",
        title: "Viridis",
        normalized: true,
        stops: &[
            (0.0, [68, 1, 84, 255]),
            (0.125, [72, 36, 117, 255]),
            (0.25, [56, 88, 140, 255]),
            (0.375, [38, 130, 142, 255]),
            (0.5, [31, 158, 137, 255]),
            (0.625, [78, 178, 101, 255]),
            (0.75, [148, 197, 56, 255]),
            (0.875, [220, 215, 30, 255]),
            (1.0, [253, 231, 37, 255]),
        ],
    },
    // One fixed scale from the stratosphere to the hottest deserts, so a
    // colour means the same temperature at every pressure level and in every
    // collection. Colour changes fastest around surface temperatures, and
    // 0 °C is a hard edge from ice white to green. The kelvin default
    // (183.15–323.15 K) lies wholly outside these stops, so it is rescaled
    // onto them exactly: 0 °C lands on 273.15 K.
    BuiltinDef {
        name: "temperature",
        title: "Temperature (°C)",
        normalized: false,
        stops: &[
            // Stratosphere and tropopause: ink to deep purple.
            (-90.0, [10, 10, 26, 255]),
            (-80.0, [26, 18, 56, 255]),
            (-70.0, [43, 22, 86, 255]),
            // 300–200 hPa: purple through violet, so jets and troughs show.
            (-60.0, [59, 31, 110, 255]),
            (-55.0, [90, 38, 135, 255]),
            (-50.0, [122, 51, 153, 255]),
            (-45.0, [124, 79, 184, 255]),
            (-40.0, [106, 102, 201, 255]),
            // 500 hPa and cold surfaces: royal blue brightening to cyan.
            (-35.0, [90, 114, 207, 255]),
            (-30.0, [58, 116, 207, 255]),
            (-25.0, [47, 138, 214, 255]),
            (-20.0, [42, 160, 218, 255]),
            (-15.0, [58, 182, 220, 255]),
            (-10.0, [95, 202, 223, 255]),
            (-6.0, [135, 217, 230, 255]),
            (-3.0, [181, 233, 239, 255]),
            // 0 °C: a hard edge from ice white to green.
            (-0.05, [227, 247, 248, 255]),
            (0.0, [79, 181, 138, 255]),
            // Surface temperatures: a new hue every couple of degrees.
            (2.0, [108, 192, 127, 255]),
            (4.0, [141, 203, 116, 255]),
            (6.0, [174, 213, 107, 255]),
            (8.0, [203, 219, 102, 255]),
            (10.0, [228, 220, 98, 255]),
            (12.0, [242, 210, 91, 255]),
            (14.0, [247, 195, 82, 255]),
            (16.0, [247, 178, 75, 255]),
            (18.0, [245, 156, 69, 255]),
            (20.0, [241, 134, 63, 255]),
            (22.0, [234, 112, 58, 255]),
            (24.0, [224, 92, 54, 255]),
            (26.0, [212, 74, 52, 255]),
            (28.0, [198, 58, 51, 255]),
            (30.0, [181, 45, 52, 255]),
            // Heat extremes: crimson to wine, then plum and orchid.
            (33.0, [156, 32, 55, 255]),
            (36.0, [130, 22, 56, 255]),
            (40.0, [100, 17, 54, 255]),
            (45.0, [125, 43, 111, 255]),
            (50.0, [179, 102, 169, 255]),
        ],
    },
    // The `temperature` palette before it widened to −90…50 °C, kept for
    // styles that name it.
    BuiltinDef {
        name: "temperature_classic",
        title: "Temperature, classic (°C)",
        normalized: false,
        stops: &[
            (-40.0, [40, 0, 120, 255]),  // deep purple (extreme cold)
            (-30.0, [0, 0, 180, 255]),   // dark blue
            (-20.0, [0, 60, 255, 255]),  // blue
            (-10.0, [0, 160, 255, 255]), // light blue
            (0.0, [0, 220, 220, 255]),   // cyan
            (10.0, [0, 200, 0, 255]),    // green
            (20.0, [200, 200, 0, 255]),  // yellow
            (30.0, [255, 128, 0, 255]),  // orange
            (40.0, [255, 0, 0, 255]),    // red
            (50.0, [180, 0, 0, 255]),    // dark red
        ],
    },
    BuiltinDef {
        name: "precipitation",
        title: "Precipitation accumulation (mm)",
        normalized: false,
        stops: &[
            (0.0, [0, 0, 0, 0]),          // transparent (no precip)
            (0.1, [170, 220, 255, 255]),  // very light blue
            (0.5, [100, 180, 255, 255]),  // light blue
            (1.0, [50, 130, 255, 255]),   // blue
            (2.0, [0, 80, 255, 255]),     // medium blue
            (5.0, [0, 40, 200, 255]),     // dark blue
            (10.0, [120, 0, 200, 255]),   // purple
            (20.0, [200, 0, 150, 255]),   // magenta
            (50.0, [255, 255, 255, 255]), // white (extreme)
        ],
    },
    // Precipitation accumulated over a forecast run (#1067: MEPS
    // `precipitation_amount_acc`, up to 66 hours): `precipitation`'s
    // colours at four times its values, 0–200 mm. A range alone cannot
    // widen `precipitation`: a data-valued palette clips at its last stop
    // (#823).
    BuiltinDef {
        name: "precipitation_accumulation",
        title: "Precipitation accumulated over a forecast run (mm)",
        normalized: false,
        stops: &[
            (0.0, [0, 0, 0, 0]),           // transparent (no precip)
            (0.4, [170, 220, 255, 255]),   // very light blue
            (2.0, [100, 180, 255, 255]),   // light blue
            (4.0, [50, 130, 255, 255]),    // blue
            (8.0, [0, 80, 255, 255]),      // medium blue
            (20.0, [0, 40, 200, 255]),     // dark blue
            (40.0, [120, 0, 200, 255]),    // purple
            (80.0, [200, 0, 150, 255]),    // magenta
            (200.0, [255, 255, 255, 255]), // white (extreme)
        ],
    },
    BuiltinDef {
        name: "precipitation_rate",
        title: "Precipitation rate (mm/h)",
        normalized: false,
        stops: &[
            (0.0, [0, 0, 0, 0]),         // transparent (no rain)
            (0.1, [200, 240, 255, 200]), // very light cyan (drizzle)
            (0.5, [100, 210, 255, 255]), // light cyan
            (1.0, [30, 170, 255, 255]),  // cyan-blue
            (2.0, [0, 120, 200, 255]),   // blue
            (4.0, [0, 180, 80, 255]),    // green
            (8.0, [200, 220, 0, 255]),   // yellow
            (15.0, [255, 140, 0, 255]),  // orange
            (30.0, [220, 0, 0, 255]),    // red (heavy)
        ],
    },
    BuiltinDef {
        name: "wind_speed",
        title: "Wind speed (m/s)",
        normalized: false,
        stops: &[
            (0.0, [0, 160, 0, 255]),    // calm green
            (5.0, [100, 200, 0, 255]),  // yellow-green
            (10.0, [200, 200, 0, 255]), // yellow
            (15.0, [255, 180, 0, 255]), // orange-yellow
            (20.0, [255, 100, 0, 255]), // orange
            (25.0, [255, 0, 0, 255]),   // red
            (30.0, [200, 0, 80, 255]),  // crimson
            (40.0, [150, 0, 150, 255]), // purple
            (50.0, [100, 0, 200, 255]), // violet
        ],
    },
    BuiltinDef {
        name: "cap_severity",
        title: "CAP alert severity",
        normalized: false,
        // Integer severity codes 0–4 with a semi-transparent alpha so the
        // alert fill overlays a basemap (Unknown=0 … Extreme=4). Used by
        // the engine-cap alert map layers (#396).
        stops: &[
            (0.0, [120, 120, 120, 200]), // Unknown — grey
            (1.0, [38, 166, 91, 200]),   // Minor — green
            (2.0, [241, 196, 15, 200]),  // Moderate — yellow
            (3.0, [230, 126, 34, 200]),  // Severe — orange
            (4.0, [192, 57, 43, 200]),   // Extreme — red
        ],
    },
    BuiltinDef {
        name: "lightning_age",
        title: "Lightning strike age (min)",
        normalized: false,
        // Value = strike age in MINUTES (#504): fresh strikes near-white,
        // aging through orange and red to dark violet at the window edge.
        stops: &[
            (0.0, [255, 255, 240, 255]), // just struck — near-white
            (5.0, [255, 236, 80, 255]),  // ≤5 min — bright yellow
            (15.0, [255, 160, 40, 255]), // orange
            (30.0, [225, 60, 50, 255]),  // red
            (45.0, [160, 40, 120, 255]), // magenta
            (60.0, [90, 30, 130, 255]),  // window edge — dark violet
        ],
    },
    BuiltinDef {
        name: "radial_velocity",
        title: "Radial velocity (m/s)",
        normalized: false,
        // Doppler radial velocity — diverging blue → white → red about
        // zero (toward/away). Stops match the ±48 m/s dealiased Nyquist
        // band used by the Nordic PVOL volumes.
        stops: &[
            (-48.0, [26, 26, 255, 255]),   // strong toward — deep blue
            (-24.0, [102, 204, 255, 255]), // toward — light blue
            (0.0, [245, 245, 245, 255]),   // stationary — near-white
            (24.0, [255, 140, 102, 255]),  // away — light red
            (48.0, [204, 0, 0, 255]),      // strong away — dark red
        ],
    },
    BuiltinDef {
        name: "pressure",
        title: "Mean sea-level pressure (hPa)",
        normalized: false,
        stops: &[
            (950.0, [85, 0, 130, 255]),     // deep low — purple
            (970.0, [40, 60, 200, 255]),    // blue
            (990.0, [0, 150, 255, 255]),    // light blue
            (1000.0, [100, 200, 150, 255]), // green
            (1013.0, [220, 220, 210, 255]), // standard atmosphere — neutral
            (1025.0, [240, 180, 80, 255]),  // orange
            (1040.0, [220, 90, 40, 255]),   // strong high — red-orange
            (1050.0, [170, 30, 30, 255]),   // extreme high — dark red
        ],
    },
    BuiltinDef {
        name: "humidity",
        title: "Relative humidity (%)",
        normalized: false,
        stops: &[
            (0.0, [150, 100, 50, 255]),   // bone dry — brown
            (20.0, [200, 160, 110, 255]), // tan
            (40.0, [230, 220, 180, 255]), // pale
            (60.0, [170, 210, 160, 255]), // light green
            (80.0, [90, 180, 140, 255]),  // green
            (100.0, [30, 120, 180, 255]), // saturated — blue
        ],
    },
    // `humidity` for a fraction in unit 1, as a source serves it
    // unconverted (engine-netcdf and engine-zarr serve CF's
    // `relative_humidity` in %): the same colours at a hundredth of the
    // values. The percent palette over 0..1 overlaps its
    // stops, so #823 would clip it to its bottom 1 % (#872), as the IR
    // palettes are kelvin-valued for the same reason.
    BuiltinDef {
        name: "humidity_fraction",
        title: "Relative humidity (fraction 0–1)",
        normalized: false,
        stops: &[
            (0.0, [150, 100, 50, 255]),
            (0.2, [200, 160, 110, 255]),
            (0.4, [230, 220, 180, 255]),
            (0.6, [170, 210, 160, 255]),
            (0.8, [90, 180, 140, 255]),
            (1.0, [30, 120, 180, 255]),
        ],
    },
    BuiltinDef {
        name: "cloud_cover",
        title: "Cloud cover (%)",
        normalized: false,
        // White with increasing opacity so the layer reads naturally over
        // a basemap: clear sky is transparent, overcast near-opaque gray.
        stops: &[
            (0.0, [255, 255, 255, 0]),     // clear — transparent
            (12.5, [255, 255, 255, 64]),   // few
            (25.0, [250, 250, 250, 110]),  // scattered
            (50.0, [240, 240, 240, 170]),  // broken
            (75.0, [225, 225, 225, 215]),  // mostly overcast
            (100.0, [200, 200, 205, 255]), // overcast — light gray
        ],
    },
    // `cloud_cover` for a fraction in unit 1, as a source serves it
    // unconverted (engine-netcdf and engine-zarr serve the CF
    // `…_area_fraction` standard names in %), as `humidity_fraction` is to
    // `humidity`.
    BuiltinDef {
        name: "cloud_cover_fraction",
        title: "Cloud cover (fraction 0–1)",
        normalized: false,
        stops: &[
            (0.0, [255, 255, 255, 0]),
            (0.125, [255, 255, 255, 64]),
            (0.25, [250, 250, 250, 110]),
            (0.5, [240, 240, 240, 170]),
            (0.75, [225, 225, 225, 215]),
            (1.0, [200, 200, 205, 255]),
        ],
    },
    // Satellite IR window brightness temperature (#819), in kelvin over
    // 180–330 K: the coldest storm tops to the warmest land. Cold (high)
    // cloud tops are bright, warm surfaces dark — the classic IR image.
    BuiltinDef {
        name: "ir_bt_grey",
        title: "IR brightness temperature (grey, K)",
        normalized: false,
        stops: &[(180.0, [255, 255, 255, 255]), (330.0, [0, 0, 0, 255])],
    },
    // The same grey for warm scenes, with a colour ramp over cold cloud
    // tops below 243 K (−30 °C), where convective storms show.
    BuiltinDef {
        name: "ir_bt_enhanced",
        title: "IR brightness temperature (enhanced cold tops, K)",
        normalized: false,
        stops: &[
            (180.0, [255, 255, 255, 255]), // overshooting tops
            (185.0, [200, 0, 200, 255]),   // magenta
            (193.0, [255, 0, 0, 255]),     // red
            (198.0, [255, 140, 0, 255]),   // orange
            (203.0, [255, 255, 0, 255]),   // yellow
            (213.0, [0, 200, 0, 255]),     // green
            (223.0, [0, 80, 255, 255]),    // blue
            (233.0, [0, 200, 255, 255]),   // cyan
            (243.0, [230, 230, 230, 255]), // light grey
            (330.0, [0, 0, 0, 255]),       // black
        ],
    },
    // Signed fields — wind components, vertical velocity, vorticity (#763).
    // Normalized, so it spans any style range; a range symmetric about zero
    // puts zero on the white midpoint. ColorBrewer RdBu, blue end low; its
    // licence notice ships in the third-party notices
    // (third-party/colorbrewer.txt).
    BuiltinDef {
        name: "diverging",
        title: "Diverging (blue–white–red)",
        normalized: true,
        stops: &[
            (0.0, [5, 48, 97, 255]),     // strong negative — dark blue
            (0.1, [33, 102, 172, 255]),  // blue
            (0.2, [67, 147, 195, 255]),  // mid blue
            (0.3, [146, 197, 222, 255]), // light blue
            (0.4, [209, 229, 240, 255]), // pale blue
            (0.5, [247, 247, 247, 255]), // zero — near-white
            (0.6, [253, 219, 199, 255]), // pale red
            (0.7, [244, 165, 130, 255]), // light red
            (0.8, [214, 96, 77, 255]),   // mid red
            (0.9, [178, 24, 43, 255]),   // red
            (1.0, [103, 0, 31, 255]),    // strong positive — dark red
        ],
    },
];

fn materialize(def: &BuiltinDef) -> Palette {
    Palette {
        name: def.name.to_string(),
        title: Some(def.title.to_string()),
        stops: def
            .stops
            .iter()
            .map(|&(value, color)| ColorStop { value, color })
            .collect(),
        interpolation: Interpolation::Linear,
        normalized: def.normalized,
        nodata_color: None,
    }
}

/// All built-in palettes, materialized once from the static table.
pub fn builtin_palettes() -> &'static [Palette] {
    static PALETTES: LazyLock<Vec<Palette>> =
        LazyLock::new(|| BUILTIN_DEFS.iter().map(materialize).collect());
    PALETTES.as_slice()
}

/// Look up a built-in palette by name.
pub fn builtin_palette(name: &str) -> Option<&'static Palette> {
    builtin_palettes().iter().find(|p| p.name == name)
}

/// All built-in palette names, in table order.
pub fn builtin_names() -> &'static [&'static str] {
    static NAMES: LazyLock<Vec<&'static str>> =
        LazyLock::new(|| BUILTIN_DEFS.iter().map(|d| d.name).collect());
    NAMES.as_slice()
}

/// Arc-wrapped built-in lookup. The `Arc`s are process-global, so cloning
/// one is refcount-cheap — this is the way to stamp a builtin into a
/// [`crate::StyleInfo`] without copying stop data.
pub fn builtin_palette_arc(name: &str) -> Option<Arc<Palette>> {
    static ARCS: LazyLock<HashMap<&'static str, Arc<Palette>>> = LazyLock::new(|| {
        BUILTIN_DEFS
            .iter()
            .map(|d| (d.name, Arc::new(materialize(d))))
            .collect()
    });
    ARCS.get(name).cloned()
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Outcome of a successful [`PaletteRegistry::insert`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteInsert {
    /// The palette was added under a new name.
    Added,
    /// The palette replaced a built-in of the same name. Allowed (it lets a
    /// deployment restyle e.g. `temperature` house-wide), but the caller
    /// should surface a warning.
    ShadowedBuiltin,
}

/// Name → palette lookup: built-ins plus user-defined entries.
///
/// Collision rules: a user palette may shadow a built-in name (the caller
/// gets [`PaletteInsert::ShadowedBuiltin`] back and should WARN); two user
/// palettes with the same name are an error.
#[derive(Clone, Debug, Default)]
pub struct PaletteRegistry {
    map: HashMap<String, Arc<Palette>>,
    user_defined: HashSet<String>,
}

impl PaletteRegistry {
    /// A registry pre-populated with every built-in palette (sharing the
    /// process-global `Arc`s — no stop data is copied).
    pub fn with_builtins() -> Self {
        let map = builtin_palettes()
            .iter()
            .map(|p| {
                let arc = builtin_palette_arc(&p.name)
                    .expect("builtin_palette_arc covers every table row");
                (p.name.clone(), arc)
            })
            .collect();
        Self {
            map,
            user_defined: HashSet::new(),
        }
    }

    /// Insert a user-defined palette. Errors on an empty name or a name
    /// already used by another user-defined palette.
    pub fn insert(&mut self, palette: Palette) -> Result<PaletteInsert, String> {
        let name = palette.name.clone();
        if name.is_empty() {
            return Err("colormap name must not be empty".to_string());
        }
        if self.user_defined.contains(&name) {
            return Err(format!("duplicate colormap name '{name}'"));
        }
        let shadowed = self.map.contains_key(&name);
        self.map.insert(name.clone(), Arc::new(palette));
        self.user_defined.insert(name);
        Ok(if shadowed {
            PaletteInsert::ShadowedBuiltin
        } else {
            PaletteInsert::Added
        })
    }

    /// Look up a palette by name.
    pub fn get(&self, name: &str) -> Option<Arc<Palette>> {
        self.map.get(name).cloned()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    /// All registered names, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.map.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rescaling onto a disjoint range keeps a display-threshold guard one
    /// ULP below its stop, leaves an overlapping range alone, and keeps an
    /// inverted range's stops ascending (#823).
    #[test]
    fn fitted_to_rescales_only_a_disjoint_range_and_keeps_the_guard() {
        let stop = |value: f64, color: [u8; 4]| ColorStop { value, color };
        let p = Palette::new(
            "guarded",
            vec![
                stop(5f64.next_down(), [0, 0, 0, 0]),
                stop(5.0, [1, 2, 3, 255]),
                stop(60.0, [4, 5, 6, 255]),
            ],
            Interpolation::Linear,
        );
        let fitted = p.fitted_to(100.0, 200.0).unwrap();
        assert_eq!(fitted.stops[1].value, 100.0);
        assert_eq!(fitted.stops[2].value, 200.0);
        assert_eq!(fitted.stops[0].value, fitted.stops[1].value.next_down());
        assert_eq!(fitted.domain_stops().len(), 2);
        assert!(p.fitted_to(0.0, 30.0).is_none(), "overlapping");
        assert!(p.fitted_to(60.0, 90.0).is_none(), "touching");
        // Inverted and disjoint (#872): rescaled by the same rule, then
        // reversed, the guard following its stop to the top, one ULP above
        // it, where it hides the values past the threshold.
        let inverted = p.fitted_to(200.0, 100.0).unwrap();
        assert!(inverted.stops.windows(2).all(|w| w[0].value <= w[1].value));
        let values: Vec<f64> = inverted.stops.iter().map(|s| s.value).collect();
        assert_eq!(values, [100.0, 200.0, 200f64.next_up()]);
        let colors: Vec<[u8; 4]> = inverted.stops.iter().map(|s| s.color).collect();
        assert_eq!(colors, [[4, 5, 6, 255], [1, 2, 3, 255], [0, 0, 0, 0]]);
        assert_eq!(inverted.domain_stops(), &inverted.stops[..2]);
        assert_eq!(inverted.domain_min_stop().unwrap().value, 100.0);
    }

    fn stop(value: f64, color: [u8; 4]) -> ColorStop {
        ColorStop { value, color }
    }

    /// The colour every value takes, sampled over and past `[lo, hi]`.
    fn sweep(p: &Palette, lo: f64, hi: f64) -> Vec<[u8; 4]> {
        (-8..=88)
            .map(|i| p.sample(lo + (hi - lo) * i as f64 / 80.0))
            .collect()
    }

    /// A linear palette reversed over a range (#872) gives every value the
    /// colour of its mirror image, a hard edge keeping each colour on its
    /// side and a guard hiding the values past the threshold at the other
    /// end. Reversed again, it is the palette it was.
    #[test]
    fn reversed_over_mirrors_linear_stops_hard_edges_and_guards() {
        let p = Palette::new(
            "edged",
            vec![
                stop(10f64.next_down(), [0, 0, 0, 0]),
                stop(10.0, [10, 0, 0, 255]),
                stop(20.0, [20, 0, 0, 255]),
                stop(20.0, [0, 20, 0, 255]),
                stop(50.0, [0, 50, 0, 255]),
            ],
            Interpolation::Linear,
        );
        let (lo, hi) = (10.0, 50.0);
        let r = p.reversed_over(lo, hi);
        assert!(r.stops.windows(2).all(|w| w[0].value <= w[1].value));
        for i in 1..80 {
            let v = lo + (hi - lo) * i as f64 / 80.0;
            if v != 40.0 {
                assert!(near(r.sample(v), p.sample(lo + hi - v)), "{v}");
            }
        }
        // The edge at 20 mirrors to 40: green below it, red above.
        assert_eq!(r.sample(39.9), [0, 20, 0, 255]);
        assert_eq!(r.sample(40.1), [20, 0, 0, 255]);
        // The threshold at 10 now ends the range at 50, still drawn.
        assert_eq!(r.sample(50.0), [10, 0, 0, 255]);
        assert_eq!(r.sample(50f64.next_up()), [0, 0, 0, 0]);
        assert_eq!(r.sample(9.0), [0, 50, 0, 255], "clamps below");
        assert_eq!(r.stops.last().unwrap().value, 50f64.next_up());
        let domain = r.domain_stops();
        assert_eq!((domain[0].value, domain[domain.len() - 1].value), (lo, hi));
        // Reversed twice, the guard is back one ULP below its stop.
        let back = r.reversed_over(lo, hi);
        assert_eq!(back.stops, p.stops);
        assert_eq!(sweep(&back, lo, hi), sweep(&p, lo, hi));
    }

    /// A step palette reversed over a range keeps each class's colour over
    /// its mirrored bounds: the class `[1, 2)` of `0..4` covers `(2, 3]`.
    #[test]
    fn reversed_over_moves_step_colours_to_their_mirrored_classes() {
        let classes = Palette::new(
            "classes",
            (0..=4)
                .map(|k| stop(k as f64, [k as u8 * 10, 0, 0, 255]))
                .collect(),
            Interpolation::Step,
        );
        let r = classes.reversed_over(0.0, 4.0);
        let values: Vec<f64> = r.stops.iter().map(|s| s.value).collect();
        assert_eq!(values, [0.0, 0.0, 1.0, 2.0, 3.0]);
        // Inside each class the mirror image's colour.
        for v in [-0.5, 0.25, 0.5, 1.5, 2.5, 3.5, 3.75, 5.0] {
            assert_eq!(r.sample(v), classes.sample(4.0 - v), "{v}");
        }
        // The top class, values of 4 and up, is the bottom one, to 0.
        assert_eq!(r.sample(-1.0), [40, 0, 0, 255]);
        assert_eq!(r.stops[0].color, [40, 0, 0, 255]);
        // Over a range past the top stop, that class starts at the range's
        // low end.
        let wider = classes.reversed_over(0.0, 6.0);
        assert_eq!((wider.stops[0].value, wider.stops[1].value), (0.0, 2.0));
    }

    /// Every fit of a range given high to low is the same range's fit low to
    /// high, reversed (#872): a normalized palette's stretch, a data-valued
    /// palette sampled as is over a range overlapping its stops, and the
    /// rescale #823 gives a range entirely outside them.
    #[test]
    fn fitted_to_an_inverted_range_reverses_every_fit() {
        let temperature = builtin_palette("temperature").unwrap();
        let viridis = builtin_palette("viridis").unwrap();
        for (p, lo, hi) in [
            (viridis, 0.0, 100.0),
            (temperature, 0.0, 40.0),
            (temperature, -100.0, 60.0),
            (temperature, 183.15, 323.15),
        ] {
            let upright = p.fitted_to(lo, hi).unwrap_or_else(|| p.clone());
            let inverted = p.fitted_to(hi, lo).expect("a high-to-low range reverses");
            assert_eq!(
                inverted.stops,
                upright.reversed_over(lo, hi).stops,
                "{} {lo}..{hi}",
                p.name
            );
            assert!(!inverted.normalized);
        }
        // Overlapping: 0 °C takes the colour of 40 °C, 30 °C of 10 °C.
        let inverted = temperature.fitted_to(40.0, 0.0).unwrap();
        for v in [0.0, 5.0, 10.5, 30.0, 39.5] {
            assert!(
                near(inverted.sample(v), temperature.sample(40.0 - v)),
                "{v}"
            );
        }
    }

    /// Colours within one unit per channel: a mirrored stop list
    /// interpolates the same colour in another order of operations.
    fn near(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 1)
    }

    /// An inverted range flips a normalized palette and keeps its stops
    /// ascending, which the samplers assume.
    #[test]
    fn stretched_onto_an_inverted_range_stays_ascending() {
        let viridis = builtin_palette("viridis").unwrap();
        let flipped = viridis.stretched(100.0, 0.0);
        assert!(flipped.stops.windows(2).all(|w| w[0].value <= w[1].value));
        assert_eq!(flipped.stops.first().unwrap().value, 0.0);
        assert_eq!(flipped.stops.last().unwrap().value, 100.0);
        assert_eq!(
            flipped.stops.first().unwrap().color,
            viridis.stops.last().unwrap().color
        );
        assert_eq!(
            flipped.stops.last().unwrap().color,
            viridis.stops.first().unwrap().color
        );
    }

    /// Pin the table against accidental edits: every palette keeps its
    /// stop count and first/last stop values.
    #[test]
    fn builtin_table_snapshot() {
        let expected: &[(&str, usize, f64, f64)] = &[
            ("radar_dbz", 13, 0.0, 70.0),
            ("radar_smhi", 32, -30.0, 70.0),
            ("radar_fmi", 12, -32.0, 58.0),
            ("radar_bookbinder", 21, -32.0, 94.5),
            ("grayscale", 2, 0.0, 1.0),
            ("viridis", 9, 0.0, 1.0),
            ("temperature", 38, -90.0, 50.0),
            ("temperature_classic", 10, -40.0, 50.0),
            ("precipitation", 9, 0.0, 50.0),
            ("precipitation_accumulation", 9, 0.0, 200.0),
            ("precipitation_rate", 9, 0.0, 30.0),
            ("wind_speed", 9, 0.0, 50.0),
            ("cap_severity", 5, 0.0, 4.0),
            ("lightning_age", 6, 0.0, 60.0),
            ("radial_velocity", 5, -48.0, 48.0),
            ("pressure", 8, 950.0, 1050.0),
            ("humidity", 6, 0.0, 100.0),
            ("humidity_fraction", 6, 0.0, 1.0),
            ("cloud_cover", 6, 0.0, 100.0),
            ("cloud_cover_fraction", 6, 0.0, 1.0),
            ("ir_bt_grey", 2, 180.0, 330.0),
            ("ir_bt_enhanced", 10, 180.0, 330.0),
            ("diverging", 11, 0.0, 1.0),
        ];
        assert_eq!(builtin_palettes().len(), expected.len());
        assert_eq!(builtin_names().len(), expected.len());
        for (name, count, first, last) in expected {
            let p = builtin_palette(name).unwrap_or_else(|| panic!("missing builtin '{name}'"));
            assert_eq!(p.stops.len(), *count, "{name} stop count");
            assert_eq!(p.stops.first().unwrap().value, *first, "{name} first stop");
            assert_eq!(p.stops.last().unwrap().value, *last, "{name} last stop");
            assert!(builtin_names().contains(name));
            // Table stops must be pre-sorted — sample()/default_range()
            // assume ascending order.
            assert!(
                p.stops.windows(2).all(|w| w[0].value <= w[1].value),
                "{name} stops not ascending"
            );
        }
    }

    /// The scaled twins keep their base palette's colours at scaled values,
    /// so an edit to one cannot drift from the other: the fraction palettes
    /// are the percent ones ÷ 100 (#872), the run accumulation is
    /// `precipitation` × 4 (#1067).
    #[test]
    fn scaled_twins_follow_their_base_palettes() {
        for (twin, base, scale) in [
            ("humidity_fraction", "humidity", 0.01),
            ("cloud_cover_fraction", "cloud_cover", 0.01),
            ("precipitation_accumulation", "precipitation", 4.0),
        ] {
            let (t, b) = (
                builtin_palette(twin).unwrap(),
                builtin_palette(base).unwrap(),
            );
            assert_eq!(t.stops.len(), b.stops.len(), "{twin}");
            assert_eq!(t.normalized, b.normalized, "{twin}");
            for (ts, bs) in t.stops.iter().zip(&b.stops) {
                assert_eq!(ts.color, bs.color, "{twin} at {}", ts.value);
                assert!(
                    (ts.value - bs.value * scale).abs() < 1e-12,
                    "{twin} at {}",
                    ts.value
                );
            }
        }
    }

    /// Spot-pin exact colors of the most load-bearing palette so a data
    /// typo in the table move is caught, not just structural drift.
    #[test]
    fn radar_dbz_exact_stops() {
        let p = builtin_palette("radar_dbz").unwrap();
        assert_eq!(p.stops[0].color, [0, 0, 0, 0]);
        assert_eq!(p.stops[2].value, 5.1);
        assert_eq!(p.stops[2].color, [0, 128, 255, 255]);
        assert_eq!(p.stops[6].value, 35.0);
        assert_eq!(p.stops[6].color, [255, 255, 0, 255]);
        assert_eq!(p.stops[12].color, [255, 255, 255, 255]);
    }

    #[test]
    fn normalized_flags_and_default_range() {
        assert!(builtin_palette("viridis").unwrap().normalized);
        assert!(builtin_palette("grayscale").unwrap().normalized);
        assert!(builtin_palette("diverging").unwrap().normalized);
        assert_eq!(builtin_palette("viridis").unwrap().default_range(), None);
        assert_eq!(
            builtin_palette("radar_dbz").unwrap().default_range(),
            Some((0.0, 70.0))
        );
        assert_eq!(
            builtin_palette("radial_velocity").unwrap().default_range(),
            Some((-48.0, 48.0))
        );
        for p in builtin_palettes() {
            assert!(!p.normalized || p.default_range().is_none());
        }
    }

    #[test]
    fn palette_new_sorts_stops() {
        let p = Palette::new(
            "reversed",
            vec![
                ColorStop {
                    value: 10.0,
                    color: [255, 0, 0, 255],
                },
                ColorStop {
                    value: 0.0,
                    color: [0, 0, 255, 255],
                },
            ],
            Interpolation::Linear,
        );
        assert_eq!(p.stops[0].value, 0.0);
        assert_eq!(p.stops[1].value, 10.0);
        assert_eq!(p.default_range(), Some((0.0, 10.0)));
    }

    #[test]
    fn palette_sample_linear_and_step() {
        let stops = vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 10.0,
                color: [100, 100, 100, 255],
            },
            ColorStop {
                value: 20.0,
                color: [200, 200, 200, 255],
            },
        ];
        let linear = Palette::new("lin", stops.clone(), Interpolation::Linear);
        assert_eq!(linear.sample(5.0), [50, 50, 50, 255]);

        let step = Palette::new("step", stops, Interpolation::Step);
        // Between stops: color of the highest stop at or below the value.
        assert_eq!(step.sample(5.0), [0, 0, 0, 255]);
        assert_eq!(step.sample(9.999), [0, 0, 0, 255]);
        assert_eq!(step.sample(10.0), [100, 100, 100, 255]);
        assert_eq!(step.sample(19.0), [100, 100, 100, 255]);
        assert_eq!(step.sample(25.0), [200, 200, 200, 255]);
        // Below the first stop clamps to the first stop's color.
        assert_eq!(step.sample(-5.0), [0, 0, 0, 255]);
    }

    #[test]
    fn registry_builtins_and_user_inserts() {
        let mut reg = PaletteRegistry::with_builtins();
        assert_eq!(reg.names().len(), builtin_palettes().len());
        assert!(reg.contains("radar_dbz"));
        assert!(reg.get("nope").is_none());

        // New user name → Added.
        let custom = Palette::new(
            "house_style",
            vec![
                ColorStop {
                    value: 0.0,
                    color: [1, 2, 3, 255],
                },
                ColorStop {
                    value: 1.0,
                    color: [4, 5, 6, 255],
                },
            ],
            Interpolation::Linear,
        );
        assert_eq!(reg.insert(custom.clone()), Ok(PaletteInsert::Added));

        // Same user name again → error.
        assert!(reg.insert(custom).is_err());

        // Shadowing a builtin → allowed, flagged.
        let shadow = Palette::new(
            "temperature",
            vec![
                ColorStop {
                    value: 0.0,
                    color: [9, 9, 9, 255],
                },
                ColorStop {
                    value: 1.0,
                    color: [8, 8, 8, 255],
                },
            ],
            Interpolation::Linear,
        );
        assert_eq!(reg.insert(shadow), Ok(PaletteInsert::ShadowedBuiltin));
        // The user palette wins on lookup.
        assert_eq!(reg.get("temperature").unwrap().stops.len(), 2);
        // Shadowing the same builtin twice is a duplicate user name.
        let shadow2 = Palette::new("temperature", vec![], Interpolation::Linear);
        assert!(reg.insert(shadow2).is_err());

        // Empty name rejected.
        assert!(reg
            .insert(Palette::new("", vec![], Interpolation::Linear))
            .is_err());
    }
}
