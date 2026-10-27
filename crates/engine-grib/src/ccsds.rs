//! Point samples of a CCSDS-packed GRIB2 field without decoding its grid
//! (#1300).
//!
//! Data representation template 5.42 entropy-codes a field with libaec in
//! reference sample intervals (RSIs) of `rsi × block_size` samples, 4096 in
//! ECMWF IFS and AIFS fields, each decodable on its own from its first bit.
//! A [`CcsdsIndex`] holds those bit offsets. [`Header::index`] builds it
//! with one decode of the payload, streamed through a small buffer, that
//! also picks out the samples its query needs; [`CcsdsIndex::sample`] then
//! decodes only the RSIs holding a query's bilinear neighbours.
//!
//! Values are bit for bit the full decode's: the same libaec stream, the
//! integer → `f32` scaling of the `grib` crate operation for operation, and
//! the [`Bilinear`] interpolation a [`crate::cache::DecodedGrid`] uses.
//! Anything else, another packing, a bitmap, another scan order or a byte
//! layout `grib` reads differently, is [`CcsdsError::Unsupported`]: the
//! caller decodes that message in full.

mod aec;
#[cfg(test)]
mod tests;

use std::io::Cursor;

use ds_core::error::DataServerError;
use grib::def::grib2::DataRepresentationTemplate;

use crate::cache::{Bilinear, Lattice};
use crate::metadata::{GridGeometry, MessageMetadata};

/// Samples per libaec call while indexing: the reused buffer holds this
/// many (48 KiB of 24-bit samples), never the field.
const CHUNK_SAMPLES: usize = 16 * 1024;

/// The largest binary scale factor `E` read here. `grib` computes `2^E` as
/// `2_f32.powi(E)`, and so does this; within ±126 every such power is an
/// exact normal `f32`, so the two cannot differ however the compiler
/// lowers them.
const MAX_BINARY_SCALE: i32 = 126;

/// The largest payload an index addresses: its bit offsets are `u32`.
const MAX_PAYLOAD_BYTES: usize = (u32::MAX / 8) as usize;

/// The least a cold prefix read fetches (#1301): enough for any usual
/// header, Sections 0 to 6 and Section 7's own, which a prefix must hold.
const MIN_PREFIX: usize = 4096;

/// How far past a prediction a partial read reaches (#1301): scaled from a
/// sibling message's index, which predicted within ±0.5 % in research; and
/// linear in the needed sample's position, −10 % to +18 %.
const SIBLING_MARGIN: f64 = 1.02;
const LINEAR_MARGIN: f64 = 1.20;

/// A read on from known data that would cover more of the message than
/// this reads all of it instead.
const MOST_OF_A_MESSAGE: f64 = 0.8;

/// A read on with no sibling to shape it predicts the rest at the denser of
/// two rates: the last [`LOCAL_RSIS`] indexed RSIs' and the rest of the
/// payload's on average, then [`READ_ON_MARGIN`]. On the IFS 0.1° and
/// 0.25° and AIFS test files that estimate fell short by at most ×1.59
/// (precipitation one RSI ahead); the average alone by up to ×4.2.
const LOCAL_RSIS: usize = 8;
const READ_ON_MARGIN: f64 = 1.6;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CcsdsError {
    /// A message this reader does not handle: decode it in full.
    #[error("not read through a CCSDS index: {0}")]
    Unsupported(&'static str),
    /// The message, its payload or the index does not hold together, or
    /// libaec failed: the full decode decides what the message holds.
    #[error("invalid CCSDS message: {0}")]
    Invalid(String),
    /// The query's deadline passed: fatal, as on every other path.
    #[error(transparent)]
    Fatal(DataServerError),
}

impl From<aec::AecError> for CcsdsError {
    fn from(error: aec::AecError) -> Self {
        Self::Invalid(error.to_string())
    }
}

fn invalid(reason: impl std::fmt::Display) -> CcsdsError {
    CcsdsError::Invalid(reason.to_string())
}

/// Section 5's simple-packing parameters and CCSDS coding.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Packing {
    aec: aec::Params,
    /// Reference value `R`.
    reference: f32,
    /// `2^E` and `10^-D`, computed as the `grib` crate computes them.
    binary: f32,
    decimal: f32,
    /// Packed values: the grid's nodes, there being no bitmap.
    values: usize,
}

impl Packing {
    /// The value of packed integer `raw`: the `grib` crate's
    /// `NonZeroSimplePackingDecoder`, `(R + raw × 2^E) × 10^-D` in `f32`.
    fn value(&self, raw: u32) -> f32 {
        let diff = raw as f32 * self.binary;
        (self.reference + diff) * self.decimal
    }
}

/// What one CCSDS message holds besides its packed values: everything a
/// point read needs, read once from Sections 1 to 7.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Header {
    lattice: Lattice,
    packing: Packing,
    /// Every node's value of a field packed with zero bits, as `grib`
    /// computes it (`R × 10^-D`); such a payload is never decoded.
    constant: Option<f32>,
    /// What a full decode reports to the parameter and discovery caches.
    metadata: MessageMetadata,
    message_len: usize,
    /// Section 7's packed bytes within the message.
    payload_start: usize,
    payload_len: usize,
}

/// A message's [`Header`] and the bit offset in its payload at which each
/// RSI starts: about 1 KB for a 0.25° global field, 6.3 KB at 0.1°.
///
/// The offsets also locate each RSI's bytes in the message,
/// `payload_start + offset / 8` on, so a reader holding only the index can
/// fetch just the RSIs it needs (#1301); and they may cover only the first
/// RSIs, as a decode of a message prefix finds them. A read past the
/// indexed RSIs is [`CcsdsError::Invalid`], never a guess.
#[derive(Debug)]
pub(crate) struct CcsdsIndex {
    header: Header,
    rsi_offsets: Box<[u32]>,
}

impl Header {
    /// Read a message's sections the way [`crate::reader::decode_message`]
    /// does, without touching its packed values. Template 5.42 with no
    /// bitmap on a regular lat/lon grid in the canonical scan order is
    /// readable; another layout is [`CcsdsError::Unsupported`], and what
    /// the full decode would reject is [`CcsdsError::Invalid`].
    pub(crate) fn parse(message: &[u8]) -> Result<Self, CcsdsError> {
        let grib2 = grib::from_reader(Cursor::new(message)).map_err(invalid)?;
        let (_, submessage) = grib2
            .iter()
            .next()
            .ok_or_else(|| invalid("no submessage"))?;
        let section7 = submessage.7.body;
        Self::read(&submessage, section7.offset, section7.size, message.len())
    }

    /// [`Self::parse`] of a message `message_len` bytes long from its first
    /// bytes `prefix` (#1301), which must hold Sections 0 to 6 and Section
    /// 7's 5-byte header; the packed values may stop anywhere. `None` when
    /// the prefix ends before that.
    ///
    /// `grib` reads a message to Section 8 and rejects one cut short, so it
    /// reads Sections 0 to 6 as they are, followed by an empty Section 7 and
    /// Section 8; the payload's position comes from the real Section 7.
    pub(crate) fn parse_prefix(
        prefix: &[u8],
        message_len: usize,
    ) -> Result<Option<Self>, CcsdsError> {
        let Some((section7, section7_len)) = find_section7(prefix)? else {
            return Ok(None);
        };
        if prefix.len() < 16
            || u64::from_be_bytes(prefix[8..16].try_into().unwrap()) != message_len as u64
        {
            return Err(invalid("message length differs from the index"));
        }
        // One field per message: Section 8 follows Section 7.
        if section7
            .checked_add(section7_len)
            .and_then(|end| end.checked_add(4))
            != Some(message_len)
        {
            return Err(invalid("Section 7 does not end the message"));
        }
        let mut stub = prefix[..section7].to_vec();
        stub.extend([0, 0, 0, 5, 7]);
        stub.extend(b"7777");
        let stub_len = stub.len() as u64;
        stub[8..16].copy_from_slice(&stub_len.to_be_bytes());
        let grib2 = grib::from_reader(Cursor::new(&stub[..])).map_err(invalid)?;
        let (_, submessage) = grib2
            .iter()
            .next()
            .ok_or_else(|| invalid("no submessage"))?;
        Self::read(&submessage, section7, section7_len, message_len).map(Some)
    }

    /// The header of a parsed submessage whose Section 7 starts at
    /// `section7` and is `section7_len` bytes long, in a message of
    /// `message_len` bytes.
    fn read<R>(
        submessage: &grib::SubMessage<'_, R>,
        section7: usize,
        section7_len: usize,
        message_len: usize,
    ) -> Result<Self, CcsdsError> {
        let section5 = submessage.section5().map_err(invalid)?;
        let DataRepresentationTemplate::_5_42(template) = &section5.payload.template else {
            return Err(CcsdsError::Unsupported("not CCSDS packing"));
        };
        if submessage
            .section6()
            .map_err(invalid)?
            .payload
            .bitmap_indicator
            != 255
        {
            return Err(CcsdsError::Unsupported("bitmap"));
        }
        // `grib` decodes only original field type 0 (floating point).
        if template.orig_field_type != 0 {
            return Err(CcsdsError::Unsupported("original field type"));
        }
        let grid_def = submessage.grid_def();
        let (lattice, canonical) = crate::reader::lattice(grid_def).map_err(invalid)?;
        if !canonical {
            return Err(CcsdsError::Unsupported("scan mode"));
        }
        let nodes = lattice
            .ni
            .checked_mul(lattice.nj)
            .ok_or_else(|| invalid("grid size overflows"))?;
        if u64::from(grid_def.num_points()) != nodes as u64 {
            return Err(invalid("grid point count mismatch"));
        }
        let values = section5.payload.num_encoded_points as usize;
        if values != nodes {
            return Err(invalid("encoded value count differs from the grid"));
        }
        let mut metadata = MessageMetadata::from_product(
            submessage.indicator().discipline,
            submessage.identification().centre_id(),
            submessage.prod_def(),
        )
        .map_err(invalid)?;
        metadata.geometry = GridGeometry::new(
            lattice.ni,
            lattice.nj,
            lattice.lon_first,
            lattice.lat_first,
            lattice.lon_inc,
            lattice.lat_inc,
        );
        metadata.uv_frame = crate::reader::uv_frame(grid_def);

        let simple = &template.simple;
        let aec = aec::Params {
            bits_per_sample: simple.num_bits.into(),
            block_size: template.block_size.into(),
            rsi: template.ref_sample_interval.into(),
            flags: template.mask.into(),
        };
        let exp = i32::from(simple.exp);
        let dec = i32::from(simple.dec);
        let constant = (simple.num_bits == 0).then(|| simple.ref_val * 10_f32.powi(-dec));
        if constant.is_none() {
            // `grib` reads each sample as `ceil(bits / 8)` bytes, most
            // significant first, unsigned: libaec must have written it so.
            if aec.bytes_per_sample() != usize::from(simple.num_bits).div_ceil(8) {
                return Err(CcsdsError::Unsupported("sample width"));
            }
            if aec.bytes_per_sample() > 1 && !aec.has(aec::DATA_MSB) {
                return Err(CcsdsError::Unsupported("least significant byte first"));
            }
            if aec.has(aec::DATA_SIGNED) {
                return Err(CcsdsError::Unsupported("signed samples"));
            }
            if aec.has(aec::PAD_RSI) {
                return Err(CcsdsError::Unsupported("padded RSIs"));
            }
            if exp.abs() > MAX_BINARY_SCALE {
                return Err(CcsdsError::Unsupported("binary scale factor"));
            }
            aec.check()?;
        }

        let payload_start = section7.checked_add(5);
        let payload_end = section7.checked_add(section7_len);
        let (Some(payload_start), Some(payload_end)) = (payload_start, payload_end) else {
            return Err(invalid("section 7 overflows"));
        };
        if payload_end > message_len || payload_start > payload_end {
            return Err(invalid("section 7 outside the message"));
        }
        if payload_end - payload_start > MAX_PAYLOAD_BYTES {
            return Err(CcsdsError::Unsupported("payload over 512 MiB"));
        }
        Ok(Self {
            lattice,
            packing: Packing {
                aec,
                reference: simple.ref_val,
                binary: 2_f32.powi(exp),
                decimal: 10_f32.powi(-dec),
                values,
            },
            constant,
            metadata,
            message_len,
            payload_start,
            payload_len: payload_end - payload_start,
        })
    }

    /// Index `message`, sampling `coords` (lon, lat) bilinearly on the way:
    /// one entropy decode of the payload, no grid. The samples are raw
    /// values, before any display conversion. Checks the query's deadline
    /// between chunks.
    pub(crate) fn index(
        self,
        message: &[u8],
        coords: &[(f64, f64)],
    ) -> Result<(CcsdsIndex, Vec<Option<f64>>), CcsdsError> {
        let plan = Plan::new(&self.lattice, coords);
        let payload = self.payload(message)?;
        if let Some(value) = self.constant {
            let index = CcsdsIndex {
                header: self,
                rsi_offsets: Box::new([]),
            };
            return Ok((index, plan.sample(|_| value)));
        }
        let params = self.packing.aec;
        let width = params.bytes_per_sample();
        let mut raw = Vec::with_capacity(plan.nodes.len());
        let offsets = aec::decode_indexed(
            &params,
            payload,
            self.packing.values,
            CHUNK_SAMPLES,
            |first, bytes| {
                ds_core::deadline::check().map_err(CcsdsError::Fatal)?;
                let end = first + bytes.len() / width;
                while let Some(&node) = plan.nodes.get(raw.len()).filter(|&&node| node < end) {
                    raw.push(packed(bytes, node - first, width));
                }
                Ok::<_, CcsdsError>(())
            },
        )?;
        if raw.len() != plan.nodes.len() {
            return Err(invalid("decode ended before the sampled nodes"));
        }
        let rsi_offsets = self.rsi_offsets(&offsets, true)?;
        let values: Vec<f32> = raw.into_iter().map(|r| self.packing.value(r)).collect();
        let index = CcsdsIndex {
            header: self,
            rsi_offsets,
        };
        Ok((index, plan.sample(|i| values[i])))
    }

    /// Section 7's packed bytes of `message`, the message this header was
    /// read from: same length, else [`CcsdsError::Invalid`].
    fn payload<'m>(&self, message: &'m [u8]) -> Result<&'m [u8], CcsdsError> {
        if message.len() != self.message_len {
            return Err(invalid("message length differs from the index"));
        }
        message
            .get(self.payload_start..self.payload_start + self.payload_len)
            .ok_or_else(|| invalid("payload outside the message"))
    }

    /// The RSI starts libaec recorded, checked against the field: one per
    /// RSI when `complete`, else those of the first RSIs; the first at bit
    /// 0, increasing, inside the payload.
    fn rsi_offsets(&self, offsets: &[usize], complete: bool) -> Result<Box<[u32]>, CcsdsError> {
        let count = self.rsi_count();
        // libaec records an RSI's start when decoding reaches it, the first
        // at bit 0; when the field ends exactly on an RSI boundary it may
        // also record where an RSI after the last would start.
        let least = if complete { count } else { 1 };
        if !(least..=count + 1).contains(&offsets.len()) {
            return Err(invalid(format!(
                "{} RSI offsets for {count} RSIs",
                offsets.len()
            )));
        }
        let offsets = &offsets[..offsets.len().min(count)];
        let bits = self.payload_len * 8;
        let ordered = offsets.first().is_none_or(|&first| first == 0)
            && offsets.windows(2).all(|pair| pair[0] < pair[1])
            && offsets.last().is_none_or(|&last| last < bits);
        if !ordered {
            return Err(invalid("RSI offsets out of order or past the payload"));
        }
        offsets
            .iter()
            .map(|&offset| u32::try_from(offset).map_err(invalid))
            .collect()
    }

    fn rsi_count(&self) -> usize {
        self.packing.values.div_ceil(self.packing.aec.rsi_samples())
    }

    /// Decode on from the RSIs `prior` indexes, or from the payload's start,
    /// over `bytes`, the message's bytes from offset `start` on (#1301):
    /// sample `coords`, recording where each RSI the decode enters starts,
    /// through the end of the last RSI they need. The bytes must begin at or
    /// before the RSI the decode starts at: the first one the points need,
    /// or the last one `prior` knows the start of. Bytes that end first are
    /// [`Extended::Short`], with what they did reveal.
    pub(crate) fn extend(
        self,
        prior: Option<&CcsdsIndex>,
        bytes: &[u8],
        start: usize,
        coords: &[(f64, f64)],
    ) -> Result<Extended, CcsdsError> {
        let plan = Plan::new(&self.lattice, coords);
        let known: &[u32] = match prior {
            Some(prior) if !prior.rsi_offsets.is_empty() => &prior.rsi_offsets[..],
            _ => &[0],
        };
        let (Some(&first), Some(&last), None) =
            (plan.nodes.first(), plan.nodes.last(), self.constant)
        else {
            // Nothing to decode: a constant field, or no point on the grid.
            let rsi_offsets = if self.constant.is_some() {
                Box::from([])
            } else {
                known.into()
            };
            let values = plan.sample(|_| self.constant.unwrap_or(f32::NAN));
            return Ok(Extended::Done {
                index: CcsdsIndex {
                    header: self,
                    rsi_offsets,
                },
                values,
            });
        };
        let params = self.packing.aec;
        let width = params.bytes_per_sample();
        let per_rsi = params.rsi_samples();
        let from_rsi = (first / per_rsi).min(known.len() - 1);
        let bit = known[from_rsi] as usize;
        let skipped = from_rsi * per_rsi;
        let end_sample = ((last / per_rsi + 1) * per_rsi).min(self.packing.values);
        let input_start = (self.payload_start + bit / 8)
            .checked_sub(start)
            .ok_or_else(|| invalid("bytes start after the RSI the decode needs"))?;
        let input_end = (self.payload_start + self.payload_len)
            .saturating_sub(start)
            .min(bytes.len());
        let mut raw = Vec::with_capacity(plan.nodes.len());
        let decoded = match bytes
            .get(input_start..input_end)
            .filter(|input| !input.is_empty())
        {
            Some(input) => aec::decode_from(
                &params,
                input,
                bit % 8,
                end_sample - skipped,
                CHUNK_SAMPLES,
                |first, chunk| {
                    ds_core::deadline::check().map_err(CcsdsError::Fatal)?;
                    let first = skipped + first;
                    let end = first + chunk.len() / width;
                    while let Some(&node) = plan.nodes.get(raw.len()).filter(|&&node| node < end) {
                        raw.push(packed(chunk, node - first, width));
                    }
                    Ok::<_, CcsdsError>(())
                },
            )?,
            None => aec::Decoded {
                samples: 0,
                offsets: vec![bit % 8],
            },
        };
        // The decode's RSI starts, counted from the input's first byte, and
        // those `prior` already knew: where both know one, they must agree.
        let base = 8 * (bit / 8);
        let mut offsets: Vec<usize> = known[..from_rsi].iter().map(|&o| o as usize).collect();
        offsets.extend(decoded.offsets.iter().map(|&o| base + o));
        let agree = known
            .iter()
            .zip(&offsets)
            .skip(from_rsi)
            .all(|(&known, &found)| known as usize == found);
        if !agree {
            return Err(invalid("RSI offsets differ from the index"));
        }
        if known.len() > offsets.len() {
            offsets.extend(known[offsets.len()..].iter().map(|&o| o as usize));
        }
        let rsi_offsets = self.rsi_offsets(&offsets, false)?;
        let index = CcsdsIndex {
            header: self,
            rsi_offsets,
        };
        if raw.len() < plan.nodes.len() {
            return Ok(Extended::Short { index });
        }
        let values: Vec<f32> = raw.into_iter().map(|r| self.packing.value(r)).collect();
        Ok(Extended::Done {
            index,
            values: plan.sample(|i| values[i]),
        })
    }
}

/// What [`Header::extend`] made of its bytes.
#[derive(Debug)]
pub(crate) enum Extended {
    /// Every point sampled, the values raw as [`Header::index`]'s; the
    /// index reaches at least the RSIs they need.
    Done {
        index: CcsdsIndex,
        values: Vec<Option<f64>>,
    },
    /// The bytes ended first: the RSIs they revealed, which
    /// [`CcsdsIndex::read_on`] predicts a read on from.
    Short { index: CcsdsIndex },
}

/// How an index that predicts a read relates to the message read (#1301).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kin {
    /// The same parameter in the same run: its RSIs lie as the message's
    /// do, within ±0.5 % in research, so [`SIBLING_MARGIN`] covers them.
    SameRun,
    /// The same parameter in another run: the same shape over the grid,
    /// less closely, with [`LINEAR_MARGIN`].
    OtherRun,
    /// Another parameter on the same grid: only where the samples lie.
    OtherParameter,
}

impl Kin {
    fn margin(self) -> f64 {
        match self {
            Self::SameRun => SIBLING_MARGIN,
            Self::OtherRun | Self::OtherParameter => LINEAR_MARGIN,
        }
    }
}

/// Which RSIs a batch of points needs decoded on one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Needs {
    first_rsi: usize,
    last_rsi: usize,
    /// The sample after the last RSI's last.
    end_sample: usize,
}

/// Where Section 7 starts in a message whose first bytes are `prefix`, and
/// how long it is: `None` when Sections 1 to 6 or Section 7's header do not
/// fit in the prefix.
fn find_section7(prefix: &[u8]) -> Result<Option<(usize, usize)>, CcsdsError> {
    let Some(indicator) = prefix.get(..16) else {
        return Ok(None);
    };
    if &indicator[..4] != b"GRIB" || indicator[7] != 2 {
        return Err(invalid("not a GRIB edition 2 message"));
    }
    let mut at = 16usize;
    loop {
        let Some(header) = prefix.get(at..at + 5) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        match header[4] {
            7 if len >= 5 => return Ok(Some((at, len))),
            1..=6 if len >= 5 => {
                at = at
                    .checked_add(len)
                    .ok_or_else(|| invalid("section length overflows"))?
            }
            _ => return Err(invalid("unexpected section before Section 7")),
        }
    }
}

impl CcsdsIndex {
    /// Sample `coords` bilinearly from `message`, the message this index was
    /// built from, decoding only the RSIs that hold their neighbours, each
    /// up to the last sample needed. Raw values, as [`Header::index`].
    pub(crate) fn sample(
        &self,
        message: &[u8],
        coords: &[(f64, f64)],
    ) -> Result<Vec<Option<f64>>, CcsdsError> {
        self.header.payload(message)?;
        self.sample_bytes(message, 0, coords)
    }

    /// [`Self::sample`] from `bytes`, the message's bytes from offset
    /// `start` on: the whole message, or the range [`Self::rsi_bytes`] names
    /// (#1301). Bytes that end before a needed sample are
    /// [`CcsdsError::Invalid`].
    pub(crate) fn sample_bytes(
        &self,
        bytes: &[u8],
        start: usize,
        coords: &[(f64, f64)],
    ) -> Result<Vec<Option<f64>>, CcsdsError> {
        let header = &self.header;
        let plan = Plan::new(&header.lattice, coords);
        if let Some(value) = header.constant {
            return Ok(plan.sample(|_| value));
        }
        let params = header.packing.aec;
        let width = params.bytes_per_sample();
        let per_rsi = params.rsi_samples();
        let payload_end = (header.payload_start + header.payload_len)
            .saturating_sub(start)
            .min(bytes.len());
        let mut values = Vec::with_capacity(plan.nodes.len());
        let mut buffer = Vec::new();
        for nodes in plan.nodes.chunk_by(|a, b| a / per_rsi == b / per_rsi) {
            let (Some(&first), Some(&last)) = (nodes.first(), nodes.last()) else {
                continue;
            };
            let rsi = first / per_rsi;
            let rsi_start = rsi * per_rsi;
            let bit = *self
                .rsi_offsets
                .get(rsi)
                .ok_or_else(|| invalid(format!("RSI {rsi} is not indexed")))?
                as usize;
            let input = (header.payload_start + bit / 8)
                .checked_sub(start)
                .and_then(|from| bytes.get(from..payload_end))
                .ok_or_else(|| invalid(format!("RSI {rsi} is outside the bytes read")))?;
            buffer.resize((last - rsi_start + 1) * width, 0);
            aec::decode_rsi(&params, input, bit % 8, &mut buffer)?;
            values.extend(nodes.iter().map(|&node| {
                header
                    .packing
                    .value(packed(&buffer, node - rsi_start, width))
            }));
        }
        Ok(plan.sample(|i| values[i]))
    }

    /// The RSIs `coords` need decoded; `None` when none: a constant field,
    /// or no point on the grid.
    pub(crate) fn needs(&self, coords: &[(f64, f64)]) -> Option<Needs> {
        let header = &self.header;
        if header.constant.is_some() {
            return None;
        }
        let per_rsi = header.packing.aec.rsi_samples();
        let plan = Plan::new(&header.lattice, coords);
        let (first, last) = (*plan.nodes.first()?, *plan.nodes.last()?);
        Some(Needs {
            first_rsi: first / per_rsi,
            last_rsi: last / per_rsi,
            end_sample: ((last / per_rsi + 1) * per_rsi).min(header.packing.values),
        })
    }

    /// Whether the index knows where RSI `rsi` ends: the next one's start,
    /// or the payload's end after the last RSI.
    fn end_bits(&self, rsi: usize) -> Option<usize> {
        match self.rsi_offsets.get(rsi + 1) {
            Some(&next) => Some(next as usize),
            None if rsi + 1 == self.header.rsi_count() && self.rsi_offsets.len() > rsi => {
                Some(self.header.payload_len * 8)
            }
            None => None,
        }
    }

    /// Whether every RSI `needs` names is indexed, start and end.
    pub(crate) fn covers(&self, needs: &Needs) -> bool {
        self.end_bits(needs.last_rsi).is_some()
    }

    /// Whether `coords` can be sampled through this index alone.
    pub(crate) fn reaches(&self, coords: &[(f64, f64)]) -> bool {
        self.needs(coords).is_none_or(|needs| self.covers(&needs))
    }

    /// The message bytes holding the RSIs `needs` names, from the first
    /// one's first byte to the last one's last (#1301); `None` when they are
    /// not all indexed.
    pub(crate) fn rsi_bytes(&self, needs: &Needs) -> Option<std::ops::Range<usize>> {
        let first = *self.rsi_offsets.get(needs.first_rsi)? as usize;
        let end = self.end_bits(needs.last_rsi)?;
        let payload_start = self.header.payload_start;
        Some(payload_start + first / 8..payload_start + end.div_ceil(8))
    }

    /// Where a read on from this index's RSIs starts and should reach to
    /// decode what `needs` names (#1301): from the first needed RSI, or the
    /// last whose start is known if that comes first, to a prediction from
    /// that last start. A sibling of the same parameter predicts from its
    /// own RSIs between the two points, scaled by the payloads' ratio, with
    /// its [`Kin`]'s margin; without one, [`Self::read_on_bits`]. A
    /// prediction past [`MOST_OF_A_MESSAGE`] reads the whole rest of the
    /// message.
    pub(crate) fn read_on(
        &self,
        needs: &Needs,
        sibling: Option<(&CcsdsIndex, Kin)>,
    ) -> std::ops::Range<usize> {
        let header = &self.header;
        let anchor = self.rsi_offsets.len().saturating_sub(1);
        let anchor_bits = self.rsi_offsets.get(anchor).map_or(0, |&o| o as usize) as f64;
        let from_rsi = needs.first_rsi.min(anchor);
        let from = self.rsi_offsets.get(from_rsi).map_or(0, |&o| o as usize);
        let payload_bits = header.payload_len as f64 * 8.0;
        let shaped = sibling
            .filter(|(_, kin)| *kin != Kin::OtherParameter)
            .and_then(|(sibling, kin)| {
                let sibling_anchor = *sibling.rsi_offsets.get(anchor)? as f64;
                let sibling_end = sibling.end_bits(needs.last_rsi)? as f64;
                let sibling_bits = sibling.header.payload_len.max(1) as f64 * 8.0;
                Some(kin.margin() * (sibling_end - sibling_anchor) * payload_bits / sibling_bits)
            });
        let rest = shaped.unwrap_or_else(|| self.read_on_bits(needs.end_sample));
        let until = header.payload_start + ((anchor_bits + rest.max(0.0)) / 8.0).ceil() as usize;
        let until = if until as f64 > MOST_OF_A_MESSAGE * header.message_len as f64 {
            header.message_len
        } else {
            until.min(header.message_len)
        };
        header.payload_start + from / 8..until
    }

    /// The bits from the last indexed RSI start to `end_sample`, predicted
    /// from this index alone: the denser of the last [`LOCAL_RSIS`] RSIs'
    /// rate and the rest of the payload's average, with [`READ_ON_MARGIN`].
    fn read_on_bits(&self, end_sample: usize) -> f64 {
        let header = &self.header;
        let per_rsi = header.packing.aec.rsi_samples();
        let anchor = self.rsi_offsets.len().saturating_sub(1);
        let bit = |rsi: usize| self.rsi_offsets.get(rsi).map_or(0.0, |&o| f64::from(o));
        let anchor_sample = anchor * per_rsi;
        let payload_bits = header.payload_len as f64 * 8.0;
        let average = (payload_bits - bit(anchor))
            / header.packing.values.saturating_sub(anchor_sample).max(1) as f64;
        let back = anchor.saturating_sub(LOCAL_RSIS);
        let local = if anchor > back {
            (bit(anchor) - bit(back)) / ((anchor - back) * per_rsi) as f64
        } else {
            0.0
        };
        let samples = end_sample.saturating_sub(anchor_sample) as f64;
        (READ_ON_MARGIN * average.max(local) * samples).min(payload_bits - bit(anchor))
    }

    /// How long a prefix of another message `message_len` bytes long, on
    /// this one's grid, should be to sample `coords` there (#1301). When this
    /// index is of the same parameter ([`Kin::SameRun`] or
    /// [`Kin::OtherRun`]) and reaches the last needed RSI, the share of its
    /// payload before that RSI's end, with the kin's margin; else the share
    /// of samples before it, with [`LINEAR_MARGIN`]. The other message's
    /// header is taken to be this one's size. At least [`MIN_PREFIX`], at
    /// most the message.
    pub(crate) fn predict_prefix(
        &self,
        coords: &[(f64, f64)],
        message_len: usize,
        kin: Kin,
    ) -> usize {
        let header = &self.header;
        let overhead = header.message_len - header.payload_len;
        let payload = message_len.saturating_sub(overhead) as f64;
        let share = match self.needs(coords) {
            None => 0.0,
            Some(needs) => match self
                .end_bits(needs.last_rsi)
                .filter(|_| kin != Kin::OtherParameter)
            {
                Some(bits) => {
                    kin.margin() * bits.div_ceil(8) as f64 / header.payload_len.max(1) as f64
                }
                None => LINEAR_MARGIN * needs.end_sample as f64 / header.packing.values as f64,
            },
        };
        let until = header.payload_start + (share * payload).ceil() as usize;
        until.max(MIN_PREFIX).min(message_len)
    }

    /// The decode settings, kept apart from the indexed RSI starts.
    pub(crate) fn header(&self) -> Header {
        self.header
    }

    /// RSI starts indexed: every RSI's when complete, else the first ones'.
    pub(crate) fn indexed_rsis(&self) -> usize {
        self.rsi_offsets.len()
    }

    /// What a full decode of the message reports to the metadata caches.
    pub(crate) fn metadata(&self) -> &MessageMetadata {
        &self.header.metadata
    }

    /// Bytes this index holds, for cache weights.
    pub(crate) fn size_bytes(&self) -> usize {
        size_of::<Self>() + std::mem::size_of_val(&*self.rsi_offsets)
    }
}

/// The nodes a batch of points reads: each point's [`Bilinear`] over
/// positions in `nodes`, the distinct nodes in storage order.
struct Plan {
    points: Vec<Option<Bilinear>>,
    nodes: Vec<usize>,
}

impl Plan {
    fn new(lattice: &Lattice, coords: &[(f64, f64)]) -> Self {
        let points: Vec<_> = coords
            .iter()
            .map(|&(lon, lat)| lattice.bilinear(lon, lat))
            .collect();
        let mut nodes: Vec<usize> = points.iter().flatten().flat_map(|b| b.nodes).collect();
        nodes.sort_unstable();
        nodes.dedup();
        let points = points
            .into_iter()
            .map(|point| point.map(|b| b.map_nodes(|node| nodes.partition_point(|&n| n < node))))
            .collect();
        Self { points, nodes }
    }

    /// Each point interpolated over `value(i)`, the value of `nodes[i]`.
    fn sample(&self, value: impl Fn(usize) -> f32) -> Vec<Option<f64>> {
        self.points
            .iter()
            .map(|point| point.as_ref()?.interpolate(|i| f64::from(value(i))))
            .collect()
    }
}

/// Packed integer `k` of libaec output read as `grib` reads it: `width`
/// bytes, most significant first.
fn packed(bytes: &[u8], k: usize, width: usize) -> u32 {
    bytes[k * width..(k + 1) * width]
        .iter()
        .fold(0, |acc, &byte| (acc << 8) | u32::from(byte))
}

/// CCSDS-encode `samples` with these coding parameters, for test fixtures.
#[cfg(test)]
pub(crate) fn encode(
    bits_per_sample: u32,
    block_size: u32,
    rsi: u32,
    flags: u32,
    samples: &[u8],
) -> Vec<u8> {
    aec::encode(
        &aec::Params {
            bits_per_sample,
            block_size,
            rsi,
            flags,
        },
        samples,
    )
}
