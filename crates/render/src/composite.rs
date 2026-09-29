//! RGB composites: three channels computed from several input planes.
//!
//! A composite image maps physical values straight to red, green and blue
//! intensities instead of running one value through a colormap. Each channel
//! reads one plane or the difference of two planes, stretches the value over
//! its range and applies a gamma. Standard satellite recipes are written this
//! way, e.g. EUMETSAT's:
//!
//! | Recipe | Red | Green | Blue |
//! |---|---|---|---|
//! | Airmass | WV6.2 - WV7.3, -25 to 0 K | IR9.7 - IR10.8, -40 to 5 K | WV6.2, 243 to 208 K |
//! | Night Microphysics | IR12.0 - IR10.8, -4 to 2 K | IR10.8 - IR3.9, 0 to 10 K | IR10.8, 243 to 293 K |
//!
//! # Conventions
//!
//! - **Range.** [`ChannelSpec::min`] is the value that gives intensity 0 and
//!   [`ChannelSpec::max`] the value that gives full intensity. `min > max`
//!   inverts the channel: Airmass blue is `min = 243, max = 208`, so colder
//!   water vapour is brighter. This is how the recipe tables write their
//!   ranges and how satpy's `crude` stretch reads `min_stretch` and
//!   `max_stretch`, so a recipe copies over without sign juggling. There is
//!   no separate invert flag.
//! - **Gamma.** The EUMETSAT convention: intensity =
//!   `clamp((v - min) / (max - min), 0, 1) ^ (1 / gamma)`, scaled to
//!   `0..=255` and rounded to the nearest byte. `gamma > 1` brightens the
//!   low end, `gamma < 1` darkens it. See [`ChannelSpec::intensity`].
//! - **Nodata.** A pixel is fully transparent (`[0, 0, 0, 0]`) when any
//!   plane that any channel reads is nodata there, including a non-finite
//!   value. Planes that no channel reads never blank a pixel.
//!
//! Input planes are decoded through [`RasterValues::value_at`], the
//! boxed-equivalent view that `colorize` is pinned against, so `U8` (with its
//! gain and offset), `F32` and `F64` planes holding the same physical values
//! compose to the same RGBA. This module has no `match` over
//! `RasterValues`: a new variant needs no change here.
//!
//! [`composite_legend_json`] and [`render_composite_legend`] describe a
//! composite without a colour bar: per channel, what it reads, its range and
//! its gamma.

use ds_core::error::DataServerError;
use ds_core::map_engine::{RasterTile, RasterValues};

use crate::{encode_rgba, font, format_tick, ImageFormat};

/// What a channel reads, as indices into the plane slice handed to
/// [`compose_rgb`] (and into [`CompositeSpec::parameters`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelSource {
    /// One plane's physical value.
    Plane(usize),
    /// The difference `planes[a] - planes[b]` of two planes' physical values.
    Difference(usize, usize),
}

impl ChannelSource {
    /// The plane indices this source reads: one for [`Self::Plane`], two for
    /// [`Self::Difference`], minuend first.
    pub fn planes(&self) -> impl Iterator<Item = usize> {
        let (a, b) = match *self {
            ChannelSource::Plane(a) => (a, None),
            ChannelSource::Difference(a, b) => (a, Some(b)),
        };
        std::iter::once(a).chain(b)
    }
}

/// One channel of an RGB composite. See the module docs for the range,
/// gamma and nodata conventions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelSpec {
    pub source: ChannelSource,
    /// Physical value that maps to intensity 0.
    pub min: f64,
    /// Physical value that maps to full intensity. `min > max` inverts the
    /// channel.
    pub max: f64,
    /// Gamma, finite and `> 0`. `1.0` is a linear stretch.
    pub gamma: f64,
}

impl ChannelSpec {
    /// Output intensity for a finite physical value:
    /// `round(255 * clamp((value - min) / (max - min), 0, 1) ^ (1 / gamma))`.
    ///
    /// The same arithmetic [`compose_rgb`] applies per pixel. The spec must
    /// be valid; [`compose_rgb`] and [`CompositeSpec::validate`] check it.
    /// This builds the gamma lookup on every call, so it suits a single value;
    /// [`compose_rgb`] builds it once per image.
    pub fn intensity(&self, value: f64) -> u8 {
        Ramp::new(self).intensity(value)
    }

    fn validate(&self, channel: &str, planes: usize) -> Result<(), DataServerError> {
        if !self.min.is_finite() || !self.max.is_finite() || self.min == self.max {
            return Err(DataServerError::Config(format!(
                "composite {channel} channel: min and max must be finite and differ, got {} and {}",
                self.min, self.max
            )));
        }
        if !self.gamma.is_finite() || self.gamma <= 0.0 {
            return Err(DataServerError::Config(format!(
                "composite {channel} channel: gamma must be finite and greater than 0, got {}",
                self.gamma
            )));
        }
        if let Some(bad) = self.source.planes().find(|&p| p >= planes) {
            return Err(DataServerError::Config(format!(
                "composite {channel} channel reads input {bad}, past the {planes} available"
            )));
        }
        Ok(())
    }
}

/// Channel names in output order, for messages and legends.
const CHANNEL_NAMES: [&str; 3] = ["red", "green", "blue"];

fn validate_channels(channels: &[ChannelSpec; 3], planes: usize) -> Result<(), DataServerError> {
    for (channel, name) in channels.iter().zip(CHANNEL_NAMES) {
        channel.validate(name, planes)?;
    }
    Ok(())
}

/// A channel's range and gamma, with the per-pixel constants hoisted.
///
/// A gamma other than 1 goes through a threshold table instead of a
/// per-pixel `powf`. Intensity `k` starts where `255 * t^(1/gamma)` reaches
/// `k - 0.5`, which is at `t = ((k - 0.5) / 255)^gamma`, so the number of
/// thresholds at or below `t` is `round(255 * t^(1/gamma))`, found by a
/// binary search over 255 values. A branch that skips `powf` for gamma 1 does
/// not help: LLVM speculates the `pow` intrinsic out of the branch, so every
/// channel paid for it and the whole compose ran about 4x slower.
struct Ramp {
    source: ChannelSource,
    min: f64,
    span: f64,
    /// `None` for gamma 1, which is a plain `round(255 * t)`.
    thresholds: Option<[f64; 255]>,
}

impl Ramp {
    fn new(spec: &ChannelSpec) -> Self {
        let thresholds = (spec.gamma != 1.0).then(|| {
            std::array::from_fn(|i| {
                // `i` is `k - 1`. The floor keeps `t = 0` at intensity 0
                // when a large gamma underflows the first thresholds.
                ((i as f64 + 0.5) / 255.0)
                    .powf(spec.gamma)
                    .max(f64::MIN_POSITIVE)
            })
        });
        Self {
            source: spec.source,
            min: spec.min,
            span: spec.max - spec.min,
            thresholds,
        }
    }

    #[inline]
    fn intensity(&self, value: f64) -> u8 {
        let t = ((value - self.min) / self.span).clamp(0.0, 1.0);
        match &self.thresholds {
            // `t` is in [0, 1], so the cast cannot saturate.
            None => (t * 255.0).round() as u8,
            // At most 255 thresholds, so the count fits.
            Some(thresholds) => thresholds.partition_point(|&th| th <= t) as u8,
        }
    }

    /// This channel's intensity at pixel `idx`, or `None` when a plane it
    /// reads is nodata there.
    #[inline]
    fn sample(&self, planes: &[&RasterValues], idx: usize) -> Option<u8> {
        let value = |p: usize| planes[p].value_at(idx).filter(|v| v.is_finite());
        let v = match self.source {
            ChannelSource::Plane(a) => value(a)?,
            ChannelSource::Difference(a, b) => value(a)? - value(b)?,
        };
        Some(self.intensity(v))
    }
}

/// Compose three channels over `planes` into a row-major RGBA8 buffer the
/// size of the planes.
///
/// Every plane must have the same width and height and hold `width *
/// height` values. A pixel is opaque with the three channel intensities, or
/// fully transparent where any plane a channel reads is nodata. See the
/// module docs for the range and gamma conventions.
///
/// Errors: [`DataServerError::Render`] for no planes or planes of different
/// sizes; [`DataServerError::Config`] for an invalid channel: non-finite or
/// equal `min`/`max`, a gamma that is not finite and positive, or a plane
/// index out of range.
pub fn compose_rgb(
    planes: &[&RasterTile],
    channels: &[ChannelSpec; 3],
) -> Result<Vec<u8>, DataServerError> {
    let (width, height) = check_planes(planes)?;
    validate_channels(channels, planes.len())?;

    let ramps = channels.each_ref().map(Ramp::new);
    let values: Vec<&RasterValues> = planes.iter().map(|p| &p.values).collect();
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    let (pixels, _) = rgba.as_chunks_mut::<4>();
    for (idx, px) in pixels.iter_mut().enumerate() {
        let Some(r) = ramps[0].sample(&values, idx) else {
            continue;
        };
        let Some(g) = ramps[1].sample(&values, idx) else {
            continue;
        };
        let Some(b) = ramps[2].sample(&values, idx) else {
            continue;
        };
        *px = [r, g, b, 255];
    }
    Ok(rgba)
}

/// The shared `(width, height)` of `planes`, or why they don't share one.
fn check_planes(planes: &[&RasterTile]) -> Result<(u32, u32), DataServerError> {
    let first = planes.first().ok_or_else(|| {
        DataServerError::Render("an RGB composite needs at least one input plane".into())
    })?;
    let (width, height) = (first.width, first.height);
    let pixels = width as usize * height as usize;
    for (i, plane) in planes.iter().enumerate() {
        if plane.width != width || plane.height != height || plane.values.len() != pixels {
            return Err(DataServerError::Render(format!(
                "composite input {i} is {}x{} with {} values, input 0 is {width}x{height}",
                plane.width,
                plane.height,
                plane.values.len()
            )));
        }
    }
    Ok((width, height))
}

/// [`compose_rgb`] encoded as an image, like
/// [`render_tile_with_background`](crate::render_tile_with_background):
/// `Some(rgb)` composites the image over that opaque colour before
/// encoding, `None` keeps the alpha channel.
pub fn render_composite(
    planes: &[&RasterTile],
    channels: &[ChannelSpec; 3],
    format: ImageFormat,
    background: Option<[u8; 3]>,
) -> Result<Vec<u8>, DataServerError> {
    ds_core::deadline::check()?;
    let mut rgba = compose_rgb(planes, channels)?;
    // `compose_rgb` succeeded, so `planes[0]` exists and sets the size.
    let (width, height) = (planes[0].width, planes[0].height);
    encode_rgba(&mut rgba, width, height, format, background)
}

/// A named RGB composite over named input parameters: what a legend
/// describes.
#[derive(Debug, Clone, PartialEq)]
pub struct CompositeSpec {
    pub name: String,
    pub title: String,
    /// The parameter behind each input plane, in the order [`compose_rgb`]
    /// receives the planes. [`ChannelSource`] indices point into this list.
    pub parameters: Vec<String>,
    /// Red, green and blue.
    pub channels: [ChannelSpec; 3],
}

impl CompositeSpec {
    /// Check every channel against [`Self::parameters`], with the same
    /// rules [`compose_rgb`] applies. Errors are
    /// [`DataServerError::Config`].
    pub fn validate(&self) -> Result<(), DataServerError> {
        validate_channels(&self.channels, self.parameters.len())
    }

    /// Display label of channel `channel` (0 = red, 1 = green, 2 = blue):
    /// the parameter name, or `"a - b"` for a difference. ASCII, so the
    /// legend font can draw it. An index past [`Self::parameters`] shows as
    /// `?`.
    ///
    /// # Panics
    /// If `channel > 2`.
    pub fn channel_label(&self, channel: usize) -> String {
        self.channel_parameters(channel).join(" - ")
    }

    /// The parameter names channel `channel` reads, minuend first; `?` for an
    /// index past [`Self::parameters`].
    fn channel_parameters(&self, channel: usize) -> Vec<&str> {
        self.channels[channel]
            .source
            .planes()
            .map(|p| self.parameters.get(p).map_or("?", String::as_str))
            .collect()
    }

    /// The unit of channel `channel`'s range: the unit every plane it reads
    /// shares. `units` runs parallel to [`Self::parameters`]; a missing
    /// entry, an empty unit, or planes with different units give `None`.
    fn channel_unit<'u>(&self, channel: usize, units: &[Option<&'u str>]) -> Option<&'u str> {
        let mut shared = None;
        for p in self.channels[channel].source.planes() {
            let unit = units
                .get(p)
                .copied()
                .flatten()
                .map(str::trim)
                .filter(|u| !u.is_empty())?;
            if shared.is_some_and(|s| s != unit) {
                return None;
            }
            shared = Some(unit);
        }
        shared
    }
}

/// Machine-readable legend for a composite: the composite variant of
/// [`legend_json`](crate::legend_json), with `channels` in place of the
/// colormap's `stops`, `min`, `max` and `interpolation`.
///
/// ```json
/// {
///   "style": "airmass",
///   "title": "Airmass RGB",
///   "channels": [
///     { "channel": "red", "label": "C08 - C10", "parameters": ["C08", "C10"],
///       "min": -25.0, "max": 0.0, "gamma": 1.0, "unit": "K" },
///     ...
///   ]
/// }
/// ```
///
/// `channels` is always red, green, blue. `parameters` holds one name for a
/// single-plane channel and two for a difference, first minus second.
/// `min` maps to intensity 0 and `max` to full intensity, so `min > max` is
/// an inverted channel; intensity is `clamp((v - min) / (max - min), 0, 1) ^
/// (1 / gamma)`. `units` runs parallel to [`CompositeSpec::parameters`] and
/// is resolved by the caller, as for `legend_json`; a channel's `unit` is
/// omitted unless every plane it reads has the same known unit.
pub fn composite_legend_json(spec: &CompositeSpec, units: &[Option<&str>]) -> serde_json::Value {
    let channels: Vec<serde_json::Value> = spec
        .channels
        .iter()
        .zip(CHANNEL_NAMES)
        .enumerate()
        .map(|(i, (channel, name))| {
            let mut entry = serde_json::json!({
                "channel": name,
                "label": spec.channel_label(i),
                "parameters": spec.channel_parameters(i),
                "min": channel.min,
                "max": channel.max,
                "gamma": channel.gamma,
            });
            if let Some(unit) = spec.channel_unit(i, units) {
                entry["unit"] = serde_json::json!(unit);
            }
            entry
        })
        .collect();
    serde_json::json!({
        "style": spec.name,
        "title": spec.title,
        "channels": channels,
    })
}

/// Render a legend image for a composite, without a colour bar.
///
/// Draws the title (or the name when the title is empty; newlines stack
/// lines), then one block per channel: a chip in the pure channel colour
/// with `"R: <label>"`, the range as `"<min> to <max> <unit>"`, and
/// `"gamma <g>"`. Same canvas, font and encoder as
/// [`render_legend`](crate::render_legend): black text on white, every write
/// clipped, so a small size crops the text instead of failing. `units` is as
/// for [`composite_legend_json`].
pub fn render_composite_legend(
    spec: &CompositeSpec,
    units: &[Option<&str>],
    width: u32,
    height: u32,
    format: ImageFormat,
) -> Result<Vec<u8>, DataServerError> {
    const PAD: u32 = 4;
    const LINE_GAP: u32 = 2; // between lines of one block
    const BLOCK_GAP: u32 = 6; // after the title and after each channel block
    const CHIP: u32 = font::GLYPH_H; // square chip, one text line tall
    const CHIP_GAP: u32 = 4; // between the chip and its text
    const TEXT: [u8; 4] = [0, 0, 0, 255];
    const BORDER: [u8; 4] = [80, 80, 80, 255];
    const CHIPS: [([u8; 4], char); 3] = [
        ([255, 0, 0, 255], 'R'),
        ([0, 255, 0, 255], 'G'),
        ([0, 0, 255, 255], 'B'),
    ];
    let line_h = font::GLYPH_H + LINE_GAP;

    let mut rgba = vec![255u8; width as usize * height as usize * 4];
    let text = |rgba: &mut [u8], x: u32, y: u32, s: &str| {
        font::draw_text(rgba, width, height, x as i32, y as i32, s, TEXT, 1);
    };

    let mut y = PAD;
    let title = if spec.title.trim().is_empty() {
        &spec.name
    } else {
        &spec.title
    };
    let mut titled = false;
    for line in title.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
        text(&mut rgba, PAD, y, line);
        y += line_h;
        titled = true;
    }
    if titled {
        y += BLOCK_GAP - LINE_GAP;
    }

    let text_x = PAD + CHIP + CHIP_GAP;
    for (i, (channel, (colour, letter))) in spec.channels.iter().zip(CHIPS).enumerate() {
        for dy in 0..CHIP {
            for dx in 0..CHIP {
                crate::set_px(&mut rgba, width, height, PAD + dx, y + dy, colour);
            }
        }
        crate::draw_rect_border(&mut rgba, width, height, PAD, y, CHIP, CHIP, BORDER);
        text(
            &mut rgba,
            text_x,
            y,
            &format!("{letter}: {}", spec.channel_label(i)),
        );
        y += line_h;

        let mut range = format!(
            "{} to {}",
            format_tick(channel.min),
            format_tick(channel.max)
        );
        if let Some(unit) = spec.channel_unit(i, units) {
            range.push(' ');
            range.push_str(unit);
        }
        text(&mut rgba, text_x, y, &range);
        y += line_h;

        text(
            &mut rgba,
            text_x,
            y,
            &format!("gamma {}", format_tick(channel.gamma)),
        );
        y += line_h + BLOCK_GAP - LINE_GAP;
    }

    encode_rgba(&mut rgba, width, height, format, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODATA_U8: u8 = 255;

    fn f64_tile(width: u32, height: u32, values: Vec<Option<f64>>) -> RasterTile {
        RasterTile {
            width,
            height,
            values: values.into(),
        }
    }

    fn channel(source: ChannelSource, min: f64, max: f64, gamma: f64) -> ChannelSpec {
        ChannelSpec {
            source,
            min,
            max,
            gamma,
        }
    }

    /// Airmass-shaped channels over planes 0..=3, with a gamma of 2 on green
    /// so the hand-computed case covers a non-linear channel.
    fn test_channels() -> [ChannelSpec; 3] {
        [
            channel(ChannelSource::Difference(0, 1), -25.0, 0.0, 1.0),
            channel(ChannelSource::Difference(2, 3), -40.0, 5.0, 2.0),
            channel(ChannelSource::Plane(0), 243.0, 208.0, 1.0),
        ]
    }

    #[test]
    fn hand_computed_difference_inverted_and_gamma_channels() {
        // Pixel 0: R = 233 - 243 = -10 over -25..0 -> 15/25 = 0.6 -> 153.
        //          G = 250 - 260 = -10 over -40..5 -> 30/45, ^(1/2) = 0.8165 -> 208.2 -> 208.
        //          B = 233 over 243..208 (inverted) -> -10/-35 = 0.2857 -> 72.86 -> 73.
        // Pixel 1: R = 250 - 200 = 50 -> clamps to 1 -> 255.
        //          G = 200 - 260 = -60 -> clamps to 0 -> 0.
        //          B = 250 is warmer than 243 -> clamps to 0 -> 0.
        // Pixel 2: plane 3 (read by green) is nodata -> transparent.
        // Pixel 3: only plane 4, which no channel reads, is nodata -> opaque.
        //          R = 208 - 208 = 0 -> 255; G = 5 - 0 = 5 -> 1 -> 255;
        //          B = 208 -> 1 -> 255.
        let wv62 = f64_tile(
            4,
            1,
            vec![Some(233.0), Some(250.0), Some(233.0), Some(208.0)],
        );
        let wv73 = f64_tile(
            4,
            1,
            vec![Some(243.0), Some(200.0), Some(243.0), Some(208.0)],
        );
        let ir97 = f64_tile(4, 1, vec![Some(250.0), Some(200.0), Some(250.0), Some(5.0)]);
        let ir108 = f64_tile(4, 1, vec![Some(260.0), Some(260.0), None, Some(0.0)]);
        let unused = f64_tile(4, 1, vec![Some(1.0), Some(1.0), Some(1.0), None]);
        let rgba = compose_rgb(&[&wv62, &wv73, &ir97, &ir108, &unused], &test_channels())
            .expect("compose");
        assert_eq!(
            rgba,
            vec![
                153, 208, 73, 255, //
                255, 0, 0, 255, //
                0, 0, 0, 0, //
                255, 255, 255, 255,
            ]
        );
    }

    #[test]
    fn intensity_follows_the_eumetsat_gamma_formula() {
        // Night Microphysics green: IR10.8 - IR3.9 over 0..10 K, gamma 0.4.
        // 5 K -> 0.5 ^ 2.5 = 0.17678 -> 45.08 -> 45.
        let g = channel(ChannelSource::Difference(0, 1), 0.0, 10.0, 0.4);
        assert_eq!(g.intensity(5.0), 45);
        assert_eq!(g.intensity(0.0), 0);
        assert_eq!(g.intensity(10.0), 255);
        assert_eq!(g.intensity(-3.0), 0, "below min clamps");
        assert_eq!(g.intensity(99.0), 255, "above max clamps");
        // Gamma 2 brightens: 0.25 ^ 0.5 = 0.5 would tie, so use 0.36 -> 0.6 -> 153.
        let bright = channel(ChannelSource::Plane(0), 0.0, 100.0, 2.0);
        assert_eq!(bright.intensity(36.0), 153);
        // Inverted: 243 gives 0, 208 gives 255, and warmer than 243 clamps to 0.
        let inv = channel(ChannelSource::Plane(0), 243.0, 208.0, 1.0);
        assert_eq!(inv.intensity(243.0), 0);
        assert_eq!(inv.intensity(208.0), 255);
        assert_eq!(inv.intensity(260.0), 0);
        assert_eq!(inv.intensity(190.0), 255);
    }

    /// The threshold table that replaces a per-pixel `powf` gives
    /// `round(255 * t^(1/gamma))` everywhere except within float noise of a
    /// rounding tie.
    #[test]
    fn gamma_threshold_table_matches_the_formula() {
        for gamma in [0.4, 0.5, 1.5, 2.0, 2.5, 50.0] {
            let spec = channel(ChannelSource::Plane(0), 0.0, 1.0, gamma);
            for i in 0..=10_000 {
                let t = i as f64 / 10_000.0;
                let exact = 255.0 * t.powf(1.0 / gamma);
                if (exact - exact.floor() - 0.5).abs() < 1e-9 {
                    continue;
                }
                assert_eq!(
                    spec.intensity(t),
                    exact.round() as u8,
                    "gamma {gamma}, t {t}"
                );
            }
        }
        // A gamma large enough to underflow the first thresholds keeps 0 at 0.
        let steep = channel(ChannelSource::Plane(0), 0.0, 1.0, 400.0);
        assert_eq!(steep.intensity(0.0), 0);
        assert_eq!(steep.intensity(1.0), 255);
    }

    /// The same physical values as `U8` (gain/offset + sentinel), `F32` and
    /// `F64` planes, in every mix, compose to identical RGBA.
    #[test]
    fn mixed_raster_value_variants_compose_identically() {
        // Physical = raw * 0.5 + 180 covers 180..=307 K in half-kelvin steps,
        // all exact in f32 and f64. Raw 255 is the U8 sentinel.
        let raws: [[u8; 6]; 4] = [
            [106, 140, 120, NODATA_U8, 60, 90],
            [126, 100, 120, 90, 60, 10],
            [140, 40, 200, 30, NODATA_U8, 70],
            [160, 160, 60, 50, 90, 80],
        ];
        let physical = |raw: u8| (raw != NODATA_U8).then_some(raw as f64 * 0.5 + 180.0);
        // Plane `r` as variant 0 = U8, 1 = F32, 2 = F64.
        let plane_as = |variant: usize, r: &[u8; 6]| {
            let values = match variant {
                0 => RasterValues::U8 {
                    data: r.to_vec(),
                    nodata: Some(NODATA_U8),
                    gain: 0.5,
                    offset: 180.0,
                },
                1 => RasterValues::F32 {
                    data: r
                        .iter()
                        .map(|&b| physical(b).map_or(f32::NAN, |v| v as f32))
                        .collect(),
                    nodata: None,
                },
                _ => r.iter().map(|&b| physical(b)).collect::<Vec<_>>().into(),
            };
            RasterTile {
                width: 3,
                height: 2,
                values,
            }
        };

        let channels = test_channels();
        let reference = {
            let tiles: Vec<RasterTile> = raws.iter().map(|r| plane_as(2, r)).collect();
            let refs: Vec<&RasterTile> = tiles.iter().collect();
            compose_rgb(&refs, &channels).expect("compose f64")
        };
        // Sanity: the fixture has opaque and transparent pixels.
        assert!(reference.as_chunks::<4>().0.iter().any(|p| p[3] == 255));
        assert!(reference.as_chunks::<4>().0.contains(&[0, 0, 0, 0]));

        // Plane i takes variant (i + shift) % 3, so every plane is tried as
        // every variant alongside the other two.
        for shift in 0..3 {
            let tiles: Vec<RasterTile> = raws
                .iter()
                .enumerate()
                .map(|(i, r)| plane_as((i + shift) % 3, r))
                .collect();
            let refs: Vec<&RasterTile> = tiles.iter().collect();
            assert_eq!(
                compose_rgb(&refs, &channels).expect("compose mixed"),
                reference,
                "variant shift {shift}"
            );
        }
    }

    #[test]
    fn non_finite_values_and_f32_sentinel_are_nodata() {
        let a = f64_tile(3, 1, vec![Some(f64::NAN), Some(f64::INFINITY), Some(1.0)]);
        let b = RasterTile {
            width: 3,
            height: 1,
            values: RasterValues::F32 {
                data: vec![1.0, 1.0, -9999.0],
                nodata: Some(-9999.0),
            },
        };
        let channels = [
            channel(ChannelSource::Plane(0), 0.0, 2.0, 1.0),
            channel(ChannelSource::Plane(0), 0.0, 2.0, 1.0),
            channel(ChannelSource::Plane(1), 0.0, 2.0, 1.0),
        ];
        let rgba = compose_rgb(&[&a, &b], &channels).expect("compose");
        assert_eq!(rgba, vec![0u8; 12], "every pixel has a nodata input");
    }

    #[test]
    fn mismatched_plane_sizes_are_rejected() {
        let channels = [
            channel(ChannelSource::Plane(0), 0.0, 1.0, 1.0),
            channel(ChannelSource::Plane(1), 0.0, 1.0, 1.0),
            channel(ChannelSource::Plane(0), 0.0, 1.0, 1.0),
        ];
        let a = f64_tile(4, 2, vec![Some(0.5); 8]);
        // Same pixel count, different shape.
        let b = f64_tile(2, 4, vec![Some(0.5); 8]);
        let err = compose_rgb(&[&a, &b], &channels).unwrap_err();
        assert!(matches!(err, DataServerError::Render(_)), "{err}");
        assert!(err.to_string().contains("input 1 is 2x4"), "{err}");
        // Right shape, wrong number of values.
        let short = f64_tile(4, 2, vec![Some(0.5); 7]);
        assert!(matches!(
            compose_rgb(&[&a, &short], &channels),
            Err(DataServerError::Render(_))
        ));
        // No planes at all.
        assert!(matches!(
            compose_rgb(&[], &channels),
            Err(DataServerError::Render(_))
        ));
    }

    #[test]
    fn invalid_channels_are_rejected() {
        let plane = f64_tile(1, 1, vec![Some(0.5)]);
        let ok = channel(ChannelSource::Plane(0), 0.0, 1.0, 1.0);
        let cases = [
            (channel(ChannelSource::Plane(0), 1.0, 1.0, 1.0), "differ"),
            (
                channel(ChannelSource::Plane(0), f64::NAN, 1.0, 1.0),
                "differ",
            ),
            (channel(ChannelSource::Plane(0), 0.0, 1.0, 0.0), "gamma"),
            (channel(ChannelSource::Plane(0), 0.0, 1.0, -1.0), "gamma"),
            (
                channel(ChannelSource::Plane(0), 0.0, 1.0, f64::INFINITY),
                "gamma",
            ),
            (
                channel(ChannelSource::Difference(0, 1), 0.0, 1.0, 1.0),
                "input 1",
            ),
        ];
        for (bad, needle) in cases {
            let err = compose_rgb(&[&plane], &[ok, bad, ok]).unwrap_err();
            assert!(matches!(err, DataServerError::Config(_)), "{err}");
            let msg = err.to_string();
            assert!(msg.contains("green") && msg.contains(needle), "{msg}");
        }
        assert!(compose_rgb(&[&plane], &[ok, ok, ok]).is_ok());
    }

    #[test]
    fn render_composite_encodes_png_and_honours_background() {
        let a = f64_tile(2, 1, vec![Some(0.0), None]);
        let channels = [channel(ChannelSource::Plane(0), 0.0, 1.0, 1.0); 3];
        let transparent =
            render_composite(&[&a], &channels, ImageFormat::Png, None).expect("encode");
        assert_eq!(
            decode_png(&transparent),
            (2, 1, vec![0, 0, 0, 255, 0, 0, 0, 0])
        );
        let opaque = render_composite(&[&a], &channels, ImageFormat::Png, Some([10, 20, 30]))
            .expect("encode");
        assert_eq!(
            decode_png(&opaque),
            (2, 1, vec![0, 0, 0, 255, 10, 20, 30, 255])
        );
    }

    /// EUMETSAT Airmass over GOES-19 ABI bands: C08 (6.2), C10 (7.3), C12
    /// (9.6), C13 (10.3).
    fn airmass() -> CompositeSpec {
        CompositeSpec {
            name: "airmass".into(),
            title: "Airmass RGB".into(),
            parameters: ["C08", "C10", "C12", "C13"].map(String::from).to_vec(),
            channels: [
                channel(ChannelSource::Difference(0, 1), -25.0, 0.0, 1.0),
                channel(ChannelSource::Difference(2, 3), -40.0, 5.0, 1.0),
                channel(ChannelSource::Plane(0), 243.0, 208.0, 1.0),
            ],
        }
    }

    #[test]
    fn composite_spec_validates_and_labels_channels() {
        let spec = airmass();
        spec.validate().expect("airmass is valid");
        assert_eq!(spec.channel_label(0), "C08 - C10");
        assert_eq!(spec.channel_label(2), "C08");
        let mut short = spec.clone();
        short.parameters.truncate(3);
        let err = short.validate().unwrap_err();
        assert!(matches!(err, DataServerError::Config(_)));
        assert!(err.to_string().contains("green"), "{err}");
        assert_eq!(short.channel_label(1), "C12 - ?");
    }

    #[test]
    fn composite_legend_json_lists_channels() {
        let spec = airmass();
        let json = composite_legend_json(&spec, &[Some("K"), Some("K"), Some("K"), Some("K")]);
        assert_eq!(
            json,
            serde_json::json!({
                "style": "airmass",
                "title": "Airmass RGB",
                "channels": [
                    { "channel": "red", "label": "C08 - C10", "parameters": ["C08", "C10"],
                      "min": -25.0, "max": 0.0, "gamma": 1.0, "unit": "K" },
                    { "channel": "green", "label": "C12 - C13", "parameters": ["C12", "C13"],
                      "min": -40.0, "max": 5.0, "gamma": 1.0, "unit": "K" },
                    { "channel": "blue", "label": "C08", "parameters": ["C08"],
                      "min": 243.0, "max": 208.0, "gamma": 1.0, "unit": "K" },
                ],
            })
        );
        // A unit is emitted only when every plane a channel reads agrees.
        let json = composite_legend_json(&spec, &[Some("K"), Some("C"), None, Some("K")]);
        let units: Vec<Option<&str>> = json["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.get("unit").and_then(|u| u.as_str()))
            .collect();
        assert_eq!(units, [None, None, Some("K")]);
        assert!(!composite_legend_json(&spec, &[])
            .to_string()
            .contains("unit"));
    }

    /// Decode a PNG to `(width, height, RGBA)`, whichever colour type the
    /// encoder picked.
    fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
        let mut reader = decoder.read_info().expect("png header");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
        let frame = reader.next_frame(&mut buf).expect("png frame");
        assert_eq!(frame.color_type, png::ColorType::Rgba);
        buf.truncate(frame.buffer_size());
        (frame.width, frame.height, buf)
    }

    #[test]
    fn composite_legend_draws_chips_and_text() {
        let spec = airmass();
        let units = [Some("K"); 4];
        let png = render_composite_legend(&spec, &units, 180, 140, ImageFormat::Png)
            .expect("legend encodes");
        let (w, h, rgba) = decode_png(&png);
        assert_eq!((w, h), (180, 140));
        let px = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        // Title line, then the gap, then one 7 px chip per block of three
        // 9 px lines plus a 4 px gap: chip centres sit at x = 7 and
        // y = 4 + 9 + 4 + 3 + k * (27 + 4).
        for (k, colour) in [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]]
            .into_iter()
            .enumerate()
        {
            assert_eq!(px(7, 20 + k as u32 * 31), colour, "chip {k}");
        }
        // Text is drawn: black pixels in the text column.
        let black = (15..w).any(|x| (0..h).any(|y| px(x, y) == [0, 0, 0, 255]));
        assert!(black, "legend text is drawn");

        // Deterministic, and the content matters.
        let again = render_composite_legend(&spec, &units, 180, 140, ImageFormat::Png).unwrap();
        assert_eq!(png, again);
        let mut other = spec.clone();
        other.channels[1].gamma = 0.4;
        let changed = render_composite_legend(&other, &units, 180, 140, ImageFormat::Png).unwrap();
        assert_ne!(png, changed, "the gamma line is drawn");
    }

    #[test]
    fn composite_legend_small_sizes_do_not_panic() {
        let spec = airmass();
        for w in [1u32, 2, 8, 40, 180] {
            for h in [1u32, 2, 8, 60, 300] {
                let png = render_composite_legend(&spec, &[], w, h, ImageFormat::Png)
                    .expect("small legend still encodes");
                assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "w={w} h={h}");
            }
        }
    }
}
