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

        let section7 = submessage.7.body;
        let payload_start = section7.offset.checked_add(5);
        let payload_end = section7.offset.checked_add(section7.size);
        let (Some(payload_start), Some(payload_end)) = (payload_start, payload_end) else {
            return Err(invalid("section 7 overflows"));
        };
        if payload_end > message.len() || payload_start > payload_end {
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
            message_len: message.len(),
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
        let rsi_offsets = self.rsi_offsets(&offsets, payload.len())?;
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
    /// RSI, the first at bit 0, increasing, inside the payload.
    fn rsi_offsets(&self, offsets: &[usize], payload_len: usize) -> Result<Box<[u32]>, CcsdsError> {
        let count = self.packing.values.div_ceil(self.packing.aec.rsi_samples());
        // libaec records an RSI's start when decoding reaches it, the first
        // at bit 0; when the field ends exactly on an RSI boundary it may
        // also record where an RSI after the last would start.
        if !(count..=count + 1).contains(&offsets.len()) {
            return Err(invalid(format!(
                "{} RSI offsets for {count} RSIs",
                offsets.len()
            )));
        }
        let offsets = &offsets[..count];
        let bits = payload_len * 8;
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
        let header = &self.header;
        let plan = Plan::new(&header.lattice, coords);
        let payload = header.payload(message)?;
        if let Some(value) = header.constant {
            return Ok(plan.sample(|_| value));
        }
        let params = header.packing.aec;
        let width = params.bytes_per_sample();
        let per_rsi = params.rsi_samples();
        let mut values = Vec::with_capacity(plan.nodes.len());
        let mut buffer = Vec::new();
        for nodes in plan.nodes.chunk_by(|a, b| a / per_rsi == b / per_rsi) {
            let (Some(&first), Some(&last)) = (nodes.first(), nodes.last()) else {
                continue;
            };
            let rsi = first / per_rsi;
            let start = rsi * per_rsi;
            let bit = *self
                .rsi_offsets
                .get(rsi)
                .ok_or_else(|| invalid(format!("RSI {rsi} is not indexed")))?;
            buffer.resize((last - start + 1) * width, 0);
            aec::decode_rsi(&params, payload, bit as usize, &mut buffer)?;
            values.extend(
                nodes
                    .iter()
                    .map(|&node| header.packing.value(packed(&buffer, node - start, width))),
            );
        }
        Ok(plan.sample(|i| values[i]))
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
