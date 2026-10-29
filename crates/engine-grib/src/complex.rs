//! Point samples of a GRIB2 field in complex packing with spatial
//! differencing (#1302), without decoding its grid.
//!
//! Data representation template 5.3, all of GFS 0.25°, codes a field's first
//! or second spatial differences in groups. Section 7 holds the extra
//! descriptors (the first values and the overall minimum), then the groups'
//! references, widths and lengths as three bit-packed arrays, then the
//! groups' values. Every value is a running sum of all the differences
//! before it, so none decodes on its own. A [`ComplexIndex`] keeps the
//! group descriptors and the decoder's state at each grid row's first value
//! (a checkpoint); the rows a query needs then decode from the first one's
//! checkpoint, over only their own data bits. A position query indexes a
//! whole message only through the rows its points read
//! ([`Header::index_to`], #1337), and later points extend that index.
//!
//! Values are bit for bit the `grib` crate's full decode: its integer
//! arithmetic (wrapping, as its release build does), its missing values and
//! bitmap, and its integer → `f32` scaling operation for operation. Points
//! sample through [`crate::cache::Bilinear`] as a decoded grid's do.
//! Another layout or scan order is [`PointError::Unsupported`], decoded in
//! full by the caller; truncated or corrupt bytes are errors, never a
//! panic.

#[cfg(test)]
mod tests;

use std::io::Cursor;
use std::ops::Range;
use std::sync::Arc;

use grib::def::grib2::DataRepresentationTemplate;

use crate::cache::Lattice;
use crate::ccsds::{
    Kin, Plan, Stub, LINEAR_MARGIN, MAX_BINARY_SCALE, MAX_PAYLOAD_BYTES, MIN_PREFIX,
    MOST_OF_A_MESSAGE, READ_ON_MARGIN,
};
use crate::metadata::{GridGeometry, MessageMetadata};
use crate::point_index::PointError;

/// Values decoded between deadline checks, about a millisecond's work.
const DEADLINE_STRIDE: u32 = 1 << 18;

/// The widest descriptor or group value read. `grib` reads them into `u32`,
/// takes a reference as `i32` and shifts by a width for the missing-value
/// masks, so 31 bits is as far as its decode is defined.
const MAX_BITS: u32 = 31;

fn invalid(reason: impl std::fmt::Display) -> PointError {
    PointError::Invalid(reason.to_string())
}

/// Section 5's simple packing: `(R + v × 2^E) × 10^-D` in `f32`, the powers
/// computed as the `grib` crate computes them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scale {
    reference: f32,
    binary: f32,
    decimal: f32,
}

impl Scale {
    /// The value of undifferenced integer `v`, NaN for a missing one: the
    /// `grib` crate's `NonZeroSimplePackingDecoder`, op for op.
    fn value(&self, v: Option<i32>) -> f32 {
        let encoded = v.map_or(f32::NAN, |v| v as f32);
        let diff = encoded * self.binary;
        (self.reference + diff) * self.decimal
    }
}

/// Section 5's complex packing and spatial differencing, and where each part
/// of Section 7's payload starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    /// NG, the number of groups.
    groups: usize,
    /// Bits per group reference.
    ref_bits: u32,
    /// A group's width is `width_ref` plus its `width_bits`-bit entry.
    width_ref: u32,
    width_bits: u32,
    /// A group's length is `len_ref` plus its `len_bits`-bit entry times
    /// `len_inc`; the last group's is `len_last`.
    len_ref: u32,
    len_inc: u32,
    len_last: u32,
    len_bits: u32,
    /// Code table 5.5: none (0), a primary (1) or both missing values (2).
    missing: u8,
    /// Code table 5.6: first (1) or second (2) order differences.
    order: u8,
    /// Octets per extra descriptor: the first values, then the minimum.
    octets: usize,
    /// Payload offsets of the references, widths, lengths and data.
    refs: usize,
    widths: usize,
    lengths: usize,
    data: usize,
}

/// One group's descriptors, as `grib` reads them.
#[derive(Debug, Clone, Copy)]
struct Group {
    reference: i32,
    width: u32,
    length: u32,
}

/// One bit-packed descriptor array, `bits` per group.
#[derive(Clone, Copy)]
struct Array<'a> {
    bytes: &'a [u8],
    bits: u32,
}

impl Array<'_> {
    /// Group `g`'s entry; `None` outside the bytes.
    fn get(&self, g: usize) -> Option<u32> {
        read_bits(self.bytes, g as u64 * u64::from(self.bits), self.bits)
    }
}

/// A message's three group descriptor arrays.
#[derive(Clone, Copy)]
struct Descriptors<'a> {
    refs: Array<'a>,
    widths: Array<'a>,
    lengths: Array<'a>,
}

impl<'a> Descriptors<'a> {
    /// The arrays in `block`: the payload's bytes from the references to the
    /// data.
    fn in_block(layout: &Layout, block: &'a [u8]) -> Self {
        let array = |from: usize, to: usize, bits: u32| Array {
            bytes: block
                .get(from - layout.refs..to - layout.refs)
                .unwrap_or(&[]),
            bits,
        };
        Self {
            refs: array(layout.refs, layout.widths, layout.ref_bits),
            widths: array(layout.widths, layout.lengths, layout.width_bits),
            lengths: array(layout.lengths, layout.data, layout.len_bits),
        }
    }

    /// Group `g`'s descriptors, with `grib`'s (release build's) arithmetic.
    fn group(&self, layout: &Layout, g: usize) -> Result<Group, PointError> {
        let outside = || invalid("group descriptors outside the bytes read");
        let reference = self.refs.get(g).ok_or_else(outside)?;
        let width = self
            .widths
            .get(g)
            .ok_or_else(outside)?
            .wrapping_add(layout.width_ref);
        let length = if g + 1 == layout.groups {
            layout.len_last
        } else {
            self.lengths
                .get(g)
                .ok_or_else(outside)?
                .wrapping_mul(layout.len_inc)
                .wrapping_add(layout.len_ref)
        };
        Ok(Group {
            // At most `MAX_BITS` bits.
            reference: reference as i32,
            width,
            length,
        })
    }
}

/// `bits` (at most 32) bits at bit `at` of `bytes`, most significant first,
/// as `grib`'s bit iterators read them; `None` past the bytes.
fn read_bits(bytes: &[u8], at: u64, bits: u32) -> Option<u32> {
    if bits == 0 {
        return Some(0);
    }
    if at.checked_add(u64::from(bits))? > 8 * bytes.len() as u64 {
        return None;
    }
    let first = (at / 8) as usize;
    let word = match bytes.get(first..first + 8) {
        Some(eight) => u64::from_be_bytes(eight.try_into().unwrap()),
        None => bytes[first..]
            .iter()
            .enumerate()
            .fold(0, |word, (i, &byte)| word | u64::from(byte) << (56 - 8 * i)),
    };
    Some((word << (at % 8) >> (64 - bits)) as u32)
}

/// Section 7's data bits held: `bytes` from the data's byte `from` on.
#[derive(Clone, Copy)]
struct Data<'a> {
    bytes: &'a [u8],
    from: usize,
}

impl Data<'_> {
    fn read(&self, bit: u64, width: u32) -> Option<u32> {
        read_bits(self.bytes, bit.checked_sub(8 * self.from as u64)?, width)
    }
}

/// Section 7's extra descriptors: the first undifferenced values and the
/// overall minimum of the differences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extra {
    first: [i32; 2],
    minimum: i32,
}

impl Extra {
    /// Read from `payload`, Section 7's payload from its start.
    fn read(layout: &Layout, payload: &[u8]) -> Self {
        let at = |i: usize| grib_int(&payload[i * layout.octets..(i + 1) * layout.octets]);
        let order = usize::from(layout.order);
        Self {
            first: [at(0), if order == 2 { at(1) } else { 0 }],
            minimum: at(order),
        }
    }
}

/// A sign-and-magnitude integer of 1 to 4 octets, as `grib` reads an extra
/// descriptor.
fn grib_int(bytes: &[u8]) -> i32 {
    let raw = bytes.iter().fold(0u32, |acc, &b| acc << 8 | u32::from(b));
    let sign = 1u32 << (8 * bytes.len() - 1);
    let magnitude = (raw & !sign) as i32;
    if raw & sign != 0 {
        -magnitude
    } else {
        magnitude
    }
}

/// The decoder's state before one packed value: a row's checkpoint, or
/// where a decode stands. All zeros is the field's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Checkpoint {
    /// The value's packed index: its node's, or its rank among the bitmap's
    /// set bits.
    packed: u32,
    /// Its group, and that group's values before it.
    group: u32,
    in_group: u32,
    /// The data bit at which the group's values start.
    group_bit: u32,
    /// Values the spatial differencing has taken, the missing ones skipped,
    /// up to its order; and the last two of them, undifferenced.
    seen: u8,
    prev1: i32,
    prev2: i32,
}

impl Checkpoint {
    /// Undifference `value`, as `grib`'s spatial differencing decoders do:
    /// the first `order` values are the extra descriptors' `first`.
    fn undifference(&mut self, order: u8, first: &[i32; 2], value: i32) -> i32 {
        let next = match (order, self.seen) {
            (_, 0) => first[0],
            (2, 1) => first[1],
            (1, _) => value.wrapping_add(self.prev1),
            _ => value
                .wrapping_add(self.prev1.wrapping_mul(2))
                .wrapping_sub(self.prev2),
        };
        self.seen = (self.seen + 1).min(order);
        self.prev2 = self.prev1;
        self.prev1 = next;
        next
    }
}

/// Set bits of a bitmap before nondecreasing positions.
struct Ranks<'a> {
    bitmap: &'a [u8],
    byte: usize,
    count: usize,
}

impl<'a> Ranks<'a> {
    fn new(bitmap: &'a [u8]) -> Self {
        Self {
            bitmap,
            byte: 0,
            count: 0,
        }
    }

    /// Set bits before bit `n`, which is in the bitmap's bytes.
    fn rank(&mut self, n: usize) -> usize {
        let byte = n / 8;
        while self.byte < byte {
            self.count += self.bitmap[self.byte].count_ones() as usize;
            self.byte += 1;
        }
        let bits = n % 8;
        self.count
            + if bits == 0 {
                0
            } else {
                (self.bitmap[byte] >> (8 - bits)).count_ones() as usize
            }
    }
}

fn is_set(bitmap: &[u8], n: usize) -> bool {
    bitmap[n / 8] & (0x80 >> (n % 8)) != 0
}

/// Each node's packed index, `None` where the bitmap masks it; `nodes`
/// sorted.
fn packed_indices(nodes: &[usize], bitmap: Option<&[u8]>) -> Vec<Option<usize>> {
    match bitmap {
        None => nodes.iter().map(|&node| Some(node)).collect(),
        Some(bitmap) => {
            let mut ranks = Ranks::new(bitmap);
            nodes
                .iter()
                .map(|&node| is_set(bitmap, node).then(|| ranks.rank(node)))
                .collect()
        }
    }
}

/// What one message holds besides its packed values, read once from
/// Sections 1 to 7: everything a point read needs except the group
/// descriptors, which the payload holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Header {
    lattice: Lattice,
    scale: Scale,
    layout: Layout,
    /// Packed values: every node's, or one per set bit of the bitmap.
    values: usize,
    /// Where the bitmap starts in the message, when Section 6 holds one.
    bitmap: Option<usize>,
    /// What a full decode reports to the parameter and discovery caches.
    metadata: MessageMetadata,
    message_len: usize,
    /// Section 7's payload within the message.
    payload_start: usize,
    payload_len: usize,
}

impl Header {
    /// Read a message's sections the way [`crate::reader::decode_message`]
    /// does, without touching its packed values: template 5.3 of the
    /// options `grib` decodes, with or without a bitmap, on a regular
    /// lat/lon grid in the canonical scan order. Another layout is
    /// [`PointError::Unsupported`], and what the full decode would reject is
    /// [`PointError::Invalid`].
    pub(crate) fn parse(message: &[u8]) -> Result<Self, PointError> {
        let grib2 = grib::from_reader(Cursor::new(message)).map_err(invalid)?;
        let (_, submessage) = grib2
            .iter()
            .next()
            .ok_or_else(|| invalid("no submessage"))?;
        let section7 = submessage.7.body;
        Self::read(
            &submessage,
            message,
            section7.offset,
            section7.size,
            message.len(),
        )
    }

    /// [`Self::parse`] of a message `message_len` bytes long from its first
    /// bytes `prefix`, which must hold Sections 0 to 6, the bitmap included,
    /// and Section 7's 5-byte header. `None` when the prefix ends before
    /// that.
    pub(crate) fn parse_prefix(
        prefix: &[u8],
        message_len: usize,
    ) -> Result<Option<Self>, PointError> {
        let Some(stub) = Stub::of(prefix, message_len)? else {
            return Ok(None);
        };
        let grib2 = grib::from_reader(Cursor::new(&stub.bytes[..])).map_err(invalid)?;
        let (_, submessage) = grib2
            .iter()
            .next()
            .ok_or_else(|| invalid("no submessage"))?;
        Self::read(
            &submessage,
            &stub.bytes,
            stub.section7,
            stub.section7_len,
            message_len,
        )
        .map(Some)
    }

    /// The header of a parsed submessage whose Sections 0 to 6 lie in
    /// `sections` at their message offsets, and whose Section 7 starts at
    /// `section7` and is `section7_len` bytes long, in a message of
    /// `message_len` bytes.
    fn read<R>(
        submessage: &grib::SubMessage<'_, R>,
        sections: &[u8],
        section7: usize,
        section7_len: usize,
        message_len: usize,
    ) -> Result<Self, PointError> {
        let section5 = submessage.section5().map_err(invalid)?;
        let DataRepresentationTemplate::_5_3(template) = &section5.payload.template else {
            return Err(PointError::Unsupported(
                "not complex packing with spatial differencing",
            ));
        };
        // What `grib` decodes: floating-point originals, general group
        // splitting, missing value management 0 to 2, first or second order
        // differences. It underflows `NG - 1` without groups.
        let complex = &template.complex;
        let simple = &template.simple;
        if template.orig_field_type != 0 {
            return Err(PointError::Unsupported("original field type"));
        }
        if complex.group_splitting_method != 1 {
            return Err(PointError::Unsupported("group splitting method"));
        }
        if complex.missing_value_management > 2 {
            return Err(PointError::Unsupported("missing value management"));
        }
        if !matches!(template.spatial_diff_order, 1 | 2) {
            return Err(PointError::Unsupported("order of spatial differencing"));
        }
        if !(1..=4).contains(&template.num_extra_desc_octets) {
            return Err(invalid("extra descriptor octets"));
        }
        if complex.num_groups == 0 {
            return Err(PointError::Unsupported("no groups"));
        }
        let descriptor_bits = [
            simple.num_bits,
            complex.num_group_width_bits,
            complex.num_group_len_bits,
        ];
        if descriptor_bits
            .iter()
            .any(|&bits| u32::from(bits) > MAX_BITS)
        {
            return Err(PointError::Unsupported("descriptor width"));
        }
        let exp = i32::from(simple.exp);
        if exp.abs() > MAX_BINARY_SCALE {
            return Err(PointError::Unsupported("binary scale factor"));
        }
        let section6 = submessage.6.body;
        let bitmap = match submessage
            .section6()
            .map_err(invalid)?
            .payload
            .bitmap_indicator
        {
            255 => None,
            0 => Some(section6.offset + 6),
            _ => return Err(PointError::Unsupported("bitmap indicator")),
        };

        let grid_def = submessage.grid_def();
        let (lattice, canonical) = crate::reader::lattice(grid_def).map_err(invalid)?;
        if !canonical {
            return Err(PointError::Unsupported("scan mode"));
        }
        let nodes = lattice
            .ni
            .checked_mul(lattice.nj)
            .ok_or_else(|| invalid("grid size overflows"))?;
        if u64::from(grid_def.num_points()) != nodes as u64 {
            return Err(invalid("grid point count mismatch"));
        }
        let values = section5.payload.num_encoded_points as usize;
        match bitmap {
            None if values != nodes => {
                return Err(invalid("encoded value count differs from the grid"));
            }
            None => {}
            // `grib`'s consistency check: the bitmap covers the grid and
            // sets one bit per encoded value.
            Some(start) => {
                let bytes = section6
                    .offset
                    .checked_add(section6.size)
                    .and_then(|end| sections.get(start..end))
                    .filter(|bytes| bytes.len() >= nodes.div_ceil(8))
                    .ok_or_else(|| invalid("bitmap shorter than the grid"))?;
                if Ranks::new(bytes).rank(nodes) != values {
                    return Err(invalid("bitmap does not count the encoded values"));
                }
            }
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

        let payload_start = section7.checked_add(5);
        let payload_end = section7.checked_add(section7_len);
        let (Some(payload_start), Some(payload_end)) = (payload_start, payload_end) else {
            return Err(invalid("section 7 overflows"));
        };
        if payload_end > message_len || payload_start > payload_end {
            return Err(invalid("section 7 outside the message"));
        }
        let payload_len = payload_end - payload_start;
        if payload_len > MAX_PAYLOAD_BYTES {
            return Err(PointError::Unsupported("payload over 512 MiB"));
        }
        let groups = complex.num_groups as usize;
        let packed = |bits: u8| (groups as u64 * u64::from(bits)).div_ceil(8);
        let refs = (u64::from(template.spatial_diff_order) + 1)
            * u64::from(template.num_extra_desc_octets);
        let widths = refs + packed(simple.num_bits);
        let lengths = widths + packed(complex.num_group_width_bits);
        let data = lengths + packed(complex.num_group_len_bits);
        if data > payload_len as u64 {
            return Err(invalid("group descriptors past Section 7"));
        }
        Ok(Self {
            lattice,
            scale: Scale {
                reference: simple.ref_val,
                binary: 2_f32.powi(exp),
                decimal: 10_f32.powi(-i32::from(simple.dec)),
            },
            layout: Layout {
                groups,
                ref_bits: simple.num_bits.into(),
                width_ref: complex.group_width_ref.into(),
                width_bits: complex.num_group_width_bits.into(),
                len_ref: complex.group_len_ref,
                len_inc: complex.group_len_inc.into(),
                len_last: complex.group_len_last,
                len_bits: complex.num_group_len_bits.into(),
                missing: complex.missing_value_management,
                order: template.spatial_diff_order,
                octets: template.num_extra_desc_octets.into(),
                // All within the payload, under 512 MiB.
                refs: refs as usize,
                widths: widths as usize,
                lengths: lengths as usize,
                data: data as usize,
            },
            values,
            bitmap,
            metadata,
            message_len,
            payload_start,
            payload_len,
        })
    }

    fn nodes(&self) -> usize {
        self.lattice.ni * self.lattice.nj
    }

    /// The message's length, the indexed length it was read with.
    pub(crate) fn message_len(&self) -> usize {
        self.message_len
    }

    /// Where the data, after every group descriptor, starts in the message.
    pub(crate) fn data_start(&self) -> usize {
        self.payload_start + self.layout.data
    }

    fn payload_end(&self) -> usize {
        self.payload_start + self.payload_len
    }

    /// An index of this message without rows yet, from `prefix`, its first
    /// bytes: the extra descriptors and the group descriptors, which come
    /// before the data. `None` when the prefix ends before the data.
    pub(crate) fn start_index(self, prefix: &[u8]) -> Option<ComplexIndex> {
        let payload = prefix.get(self.payload_start..self.data_start())?;
        Some(ComplexIndex {
            header: self,
            extra: Extra::read(&self.layout, payload),
            descriptors: Arc::from(&payload[self.layout.refs..]),
            rows: Box::new([]),
        })
    }

    /// Index `message`, sampling `coords` (lon, lat) bilinearly on the way:
    /// one decode of every packed value, no grid, checking every group's
    /// data is there as the full decode does. The samples are raw values,
    /// before any display conversion. Checks the query's deadline as it
    /// goes.
    pub(crate) fn index(
        self,
        message: &[u8],
        coords: &[(f64, f64)],
    ) -> Result<(ComplexIndex, Vec<Option<f64>>), PointError> {
        if message.len() != self.message_len {
            return Err(invalid("message length differs from the index"));
        }
        let start = self
            .start_index(message)
            .ok_or_else(|| invalid("payload outside the message"))?;
        let plan = Plan::new(&self.lattice, coords);
        let (rows, values) = start.decode_nodes(message, 0, &plan.nodes, true)?;
        let index = ComplexIndex {
            rows: rows.into(),
            ..start
        };
        Ok((index, plan.sample(|i| values[i])))
    }

    /// [`Self::index`] only through the rows `coords` read (#1337): their
    /// values, decoded from the field's start through the start of the row
    /// after the last one they read, and the checkpoints of the rows passed.
    /// A later point past them reads on from the last
    /// ([`ComplexIndex::read_message`]). The groups past those rows are not
    /// decoded, so not checked, as in a prefix read (#1302).
    pub(crate) fn index_to(
        self,
        message: &[u8],
        coords: &[(f64, f64)],
    ) -> Result<(ComplexIndex, Vec<Option<f64>>), PointError> {
        let start = self
            .start_index(message)
            .ok_or_else(|| invalid("payload outside the message"))?;
        let (values, extended) = start.read_message(message, coords)?;
        Ok((extended.unwrap_or(start), values))
    }

    /// Where a read on from a prefix that ends before the data should reach
    /// to sample `coords` (#1302): the descriptors, then the share of the
    /// data before the end of the last row the points read, with
    /// [`READ_ON_MARGIN`]; past [`MOST_OF_A_MESSAGE`], the message's end.
    pub(crate) fn predict_end(&self, coords: &[(f64, f64)]) -> usize {
        let plan = Plan::new(&self.lattice, coords);
        let rows = plan
            .nodes
            .last()
            .map_or(0, |&last| last / self.lattice.ni + 1);
        let share = rows as f64 / self.lattice.nj as f64;
        let data = (self.payload_end() - self.data_start()) as f64;
        let until = self.data_start() + (READ_ON_MARGIN * share * data).ceil() as usize;
        if until as f64 > MOST_OF_A_MESSAGE * self.message_len as f64 {
            self.message_len
        } else {
            until.clamp(self.data_start() + 1, self.message_len)
        }
    }
}

/// A message's [`Header`], its descriptors, and the decoder's state at each
/// grid row's first value: about 20 KB of checkpoints and 75 to 100 KB of
/// descriptors for a GFS 0.25° field.
///
/// The checkpoints may cover only the first rows, as a decode of a message
/// prefix finds them; a read from a later row decodes on from the last one.
/// Every read decodes from a checkpoint at or before the values it needs, so
/// the data it needs is one span of the message ([`Self::span`]).
#[derive(Debug)]
pub(crate) struct ComplexIndex {
    header: Header,
    extra: Extra,
    /// The group references, widths and lengths as packed: the payload from
    /// the references to the data. Shared with the indexes this one extends
    /// to.
    descriptors: Arc<[u8]>,
    rows: Box<[Checkpoint]>,
}

impl ComplexIndex {
    fn descriptors(&self) -> Descriptors<'_> {
        Descriptors::in_block(&self.header.layout, &self.descriptors)
    }

    /// The decode settings.
    pub(crate) fn header(&self) -> &Header {
        &self.header
    }

    /// What a full decode of the message reports to the metadata caches.
    pub(crate) fn metadata(&self) -> &MessageMetadata {
        &self.header.metadata
    }

    /// Rows whose checkpoints are indexed: every row's when complete, else
    /// the first ones'.
    pub(crate) fn indexed_rows(&self) -> usize {
        self.rows.len()
    }

    /// Whether every row's checkpoint is indexed, as [`Header::index`]
    /// indexes them: any point can then be read from its own rows.
    pub(crate) fn complete(&self) -> bool {
        self.rows.len() >= self.header.lattice.nj
    }

    /// Whether Section 6 holds a bitmap: a read must then hold it too, to
    /// place the needed nodes among the packed values.
    pub(crate) fn bitmapped(&self) -> bool {
        self.header.bitmap.is_some()
    }

    /// Bytes this index holds, for cache weights.
    pub(crate) fn size_bytes(&self) -> usize {
        size_of::<Self>()
            + 2 * size_of::<usize>()
            + self.descriptors.len()
            + std::mem::size_of_val(&*self.rows)
    }

    /// Whether the index has the checkpoint of every row `coords` read: a
    /// read from those rows extends nothing.
    pub(crate) fn reaches(&self, coords: &[(f64, f64)]) -> bool {
        let plan = Plan::new(&self.header.lattice, coords);
        plan.nodes
            .last()
            .is_none_or(|&last| self.rows.len() > last / self.header.lattice.ni)
    }

    /// The bitmap's bytes in `part`, the message's bytes from offset
    /// `start` on: `None` without a bitmap, an error when the part does not
    /// hold it.
    pub(crate) fn bitmap_in<'a>(
        &self,
        part: &'a [u8],
        start: usize,
    ) -> Result<Option<&'a [u8]>, PointError> {
        let Some(at) = self.header.bitmap else {
            return Ok(None);
        };
        let len = self.header.nodes().div_ceil(8);
        at.checked_sub(start)
            .and_then(|from| part.get(from..from + len))
            .map(Some)
            .ok_or_else(|| invalid("bitmap outside the bytes read"))
    }

    /// Sample `coords` from `message`, the whole message this index was
    /// built from, decoding from the checkpoint of the first row they read
    /// through the last value they need. Raw values, as [`Header::index`].
    pub(crate) fn sample(
        &self,
        message: &[u8],
        coords: &[(f64, f64)],
    ) -> Result<Vec<Option<f64>>, PointError> {
        self.read_message(message, coords).map(|(values, _)| values)
    }

    /// [`Self::read`] from `message`, the whole message this index is of:
    /// raw values, and this index extended by the rows the decode passed
    /// when the points lie past those indexed (#1337). A message of another
    /// length is [`PointError::Invalid`].
    pub(crate) fn read_message(
        &self,
        message: &[u8],
        coords: &[(f64, f64)],
    ) -> Result<(Vec<Option<f64>>, Option<ComplexIndex>), PointError> {
        if message.len() != self.header.message_len {
            return Err(invalid("message length differs from the index"));
        }
        self.read(message, 0, coords)
    }

    /// Sample `coords` from `part`, the message's bytes from offset `start`
    /// on, which hold [`Self::span`] and, for a bitmapped field, the
    /// bitmap. Raw values, and this index extended by the rows the decode
    /// passed, when it passed any ([`Self::span`] reads on from the last
    /// row indexed). Bytes that end before a needed value are
    /// [`PointError::Invalid`].
    pub(crate) fn read(
        &self,
        part: &[u8],
        start: usize,
        coords: &[(f64, f64)],
    ) -> Result<(Vec<Option<f64>>, Option<ComplexIndex>), PointError> {
        let plan = Plan::new(&self.header.lattice, coords);
        let (rows, values) = self.decode_nodes(part, start, &plan.nodes, false)?;
        let extended = (!rows.is_empty()).then(|| ComplexIndex {
            header: self.header,
            extra: self.extra,
            descriptors: self.descriptors.clone(),
            rows: self.rows.iter().copied().chain(rows).collect(),
        });
        Ok((plan.sample(|i| values[i]), extended))
    }

    /// The message bytes a read of `coords` decodes (#1302): from the data
    /// of the checkpoint it starts at, the first row it reads or the last
    /// row indexed before that, through the last value it needs. A
    /// bitmapped field's needed values lie where its `bitmap` says. An empty
    /// span reads nothing: no point on the grid, every needed node masked,
    /// or only constant groups.
    pub(crate) fn span(
        &self,
        coords: &[(f64, f64)],
        bitmap: Option<&[u8]>,
    ) -> Result<Range<usize>, PointError> {
        if self.bitmapped() && bitmap.is_none() {
            return Err(invalid("a bitmapped field's span needs its bitmap"));
        }
        let plan = Plan::new(&self.header.lattice, coords);
        let targets = packed_indices(&plan.nodes, bitmap);
        let data_start = self.header.data_start();
        let first = plan.nodes.iter().zip(&targets).find(|(_, p)| p.is_some());
        let (Some((&first_node, _)), Some(last)) = (first, targets.iter().flatten().max()) else {
            return Ok(data_start..data_start);
        };
        let from = self.start_at(first_node / self.header.lattice.ni);
        let (start_bit, _) = self.locate(&from, from.packed as usize)?;
        let (last_bit, width) = self.locate(&from, *last)?;
        let end_bit = last_bit + u64::from(width);
        Ok(data_start + (start_bit / 8) as usize..data_start + end_bit.div_ceil(8) as usize)
    }

    /// The prefix holding what a read of `coords` needs, from the
    /// checkpoints alone: through the data of the first row after the last
    /// one they read, or the whole message for its last row. What a
    /// bitmapped field reads, whose needed values only its bitmap places
    /// (#1302). `None` when the index does not reach that row.
    pub(crate) fn prefix_bound(&self, coords: &[(f64, f64)]) -> Option<usize> {
        let plan = Plan::new(&self.header.lattice, coords);
        let rows = plan
            .nodes
            .last()
            .map_or(0, |&last| last / self.header.lattice.ni + 1);
        if rows >= self.header.lattice.nj {
            return Some(self.header.message_len);
        }
        let next = self.rows.get(rows)?;
        // Rows the bitmap masks to the end start after the last value.
        if next.packed as usize >= self.header.values {
            return Some(self.header.payload_end());
        }
        let (bit, _) = self.locate(next, next.packed as usize).ok()?;
        Some((self.header.data_start() + bit.div_ceil(8) as usize).min(self.header.message_len))
    }

    /// How long a prefix of another message `message_len` bytes long, on
    /// this one's grid, should be to sample `coords` there (#1302). When
    /// this index is of the same parameter ([`Kin::SameRun`] or
    /// [`Kin::OtherRun`]) and has the checkpoint of the row after the last
    /// one they read, its own share of the message through that row's
    /// start, with the kin's margin; else its header and descriptors' share
    /// plus the share of the data before that row, with [`LINEAR_MARGIN`].
    /// At least [`MIN_PREFIX`], at most the message.
    pub(crate) fn predict_prefix(
        &self,
        coords: &[(f64, f64)],
        message_len: usize,
        kin: Kin,
    ) -> usize {
        let header = &self.header;
        let own = header.message_len.max(1) as f64;
        let plan = Plan::new(&header.lattice, coords);
        let rows = plan
            .nodes
            .last()
            .map_or(0, |&last| last / header.lattice.ni + 1);
        let shaped = (kin != Kin::OtherParameter)
            .then(|| self.prefix_bound(coords))
            .flatten()
            .map(|bound| kin.margin() * bound as f64 / own);
        let share = shaped.unwrap_or_else(|| {
            let data_start = header.data_start() as f64;
            let data = (header.payload_end() as f64 - data_start)
                * (rows.min(header.lattice.nj) as f64 / header.lattice.nj as f64);
            LINEAR_MARGIN * (data_start + data) / own
        });
        ((share * message_len as f64).ceil() as usize)
            .max(MIN_PREFIX)
            .min(message_len)
    }

    /// The checkpoint a read of row `row` on starts from: that row's, or the
    /// last indexed before it; the field's start without rows.
    fn start_at(&self, row: usize) -> Checkpoint {
        match self.rows.len() {
            0 => Checkpoint::default(),
            rows => self.rows[row.min(rows - 1)],
        }
    }

    /// The data bit at which packed value `packed` starts and its group's
    /// width, from the group descriptors alone, walking from checkpoint
    /// `from`, at or before it. A value whose bits would pass the data is
    /// [`PointError::Invalid`].
    fn locate(&self, from: &Checkpoint, packed: usize) -> Result<(u64, u32), PointError> {
        let layout = &self.header.layout;
        let descriptors = self.descriptors();
        let data_bits = 8 * (self.header.payload_len - layout.data) as u64;
        let past = || invalid("a value past the data");
        let mut group = from.group as usize;
        // The packed index of the group's first value, at most `packed`.
        let mut first = (from.packed as usize)
            .checked_sub(from.in_group as usize)
            .filter(|&first| first <= packed)
            .ok_or_else(|| invalid("checkpoint after the value"))?;
        let mut bit = u64::from(from.group_bit);
        loop {
            if group >= layout.groups {
                return Err(invalid("the groups end before the field"));
            }
            let g = descriptors.group(layout, group)?;
            if packed - first < g.length as usize {
                let start = ((packed - first) as u64)
                    .checked_mul(u64::from(g.width))
                    .and_then(|offset| bit.checked_add(offset))
                    .filter(|start| {
                        start
                            .checked_add(u64::from(g.width))
                            .is_some_and(|end| end <= data_bits)
                    })
                    .ok_or_else(past)?;
                return Ok((start, g.width));
            }
            first += g.length as usize;
            bit = (u64::from(g.length) * u64::from(g.width))
                .checked_add(bit)
                .filter(|&bit| bit <= data_bits)
                .ok_or_else(past)?;
            group += 1;
        }
    }

    /// Section 7's data in `part`, the message's bytes from offset `start`
    /// on, up to the payload's end.
    fn data_in<'a>(&self, part: &'a [u8], start: usize) -> Data<'a> {
        let data_start = self.header.data_start();
        let end = self
            .header
            .payload_end()
            .saturating_sub(start)
            .min(part.len());
        match data_start.checked_sub(start) {
            Some(skip) => Data {
                bytes: part.get(skip..end).unwrap_or(&[]),
                from: 0,
            },
            None => Data {
                bytes: part.get(..end).unwrap_or(&[]),
                from: start - data_start,
            },
        }
    }

    /// The values of `nodes` (sorted, distinct) from `part`, the message's
    /// bytes from offset `start` on, and the checkpoints of the rows after
    /// those indexed that the decode reached. The decode starts at the
    /// checkpoint of the first row with a needed value, or the last indexed
    /// before it, and runs through the last needed value, on to the start
    /// of the row after the last node's, masked or not, when `part` holds
    /// the values before it ([`Self::decode_end`]): the index then has the
    /// checkpoint of every row `nodes` lie in (#1337). Or through every
    /// value when `whole` (from the field's start: an index without rows).
    fn decode_nodes(
        &self,
        part: &[u8],
        start: usize,
        nodes: &[usize],
        whole: bool,
    ) -> Result<(Vec<Checkpoint>, Vec<f32>), PointError> {
        let header = &self.header;
        let (ni, nj) = (header.lattice.ni, header.lattice.nj);
        let bitmap = self.bitmap_in(part, start)?;
        let targets: Vec<(usize, usize)> = packed_indices(nodes, bitmap)
            .into_iter()
            .enumerate()
            .filter_map(|(slot, packed)| Some((packed?, slot)))
            .collect();
        // Masked nodes are NaN, as `grib` gives them.
        let mut values = vec![f32::NAN; nodes.len()];
        let data = self.data_in(part, start);
        let (from, end) = match (targets.first(), targets.last()) {
            _ if whole => (Checkpoint::default(), header.values),
            (Some(&(_, first)), Some(&(last, _))) => {
                let from = self.start_at(nodes[first] / ni);
                let next_row = nodes[nodes.len() - 1] / ni + 1;
                (from, self.decode_end(&from, last, next_row, bitmap, &data))
            }
            _ => return Ok((Vec::new(), values)),
        };
        let mut rows = Rows {
            found: Vec::new(),
            next: self.rows.len(),
            start: 0,
            ranks: bitmap.map(Ranks::new),
            ni,
            nj,
        };
        rows.start = rows.row_start();
        let mut target = 0;
        let at = self.walk(data, from, end, |before, value| {
            rows.pass(before);
            if let Some(&(_, slot)) = targets
                .get(target)
                .filter(|(packed, _)| *packed == before.packed as usize)
            {
                values[slot] = header.scale.value(value);
                target += 1;
            }
        })?;
        rows.pass(&at);
        if target != targets.len() {
            return Err(invalid("decode ended before the sampled values"));
        }
        if whole && at.in_group > 0 {
            // `grib` reads every value of the last group it enters.
            let group = self
                .descriptors()
                .group(&header.layout, at.group as usize)?;
            let bits = u64::from(at.group_bit) + u64::from(group.length) * u64::from(group.width);
            if bits > 8 * data.bytes.len() as u64 {
                return Err(invalid("data ends inside the last group"));
            }
        }
        Ok((rows.found, values))
    }

    /// Where a decode from `from` for values through packed index `last`
    /// stops (exclusive): just after it, or at the start of row `next_row`
    /// (the field's end past the last row) when `data` holds every value
    /// before that row. Its checkpoint then bounds a later prefix read of
    /// the rows before it ([`Self::prefix_bound`]).
    fn decode_end(
        &self,
        from: &Checkpoint,
        last: usize,
        next_row: usize,
        bitmap: Option<&[u8]>,
        data: &Data<'_>,
    ) -> usize {
        let header = &self.header;
        let node = next_row.min(header.lattice.nj) * header.lattice.ni;
        let next = match bitmap {
            Some(bitmap) => Ranks::new(bitmap).rank(node),
            None => node,
        };
        let end_bit = if next >= header.values {
            header
                .values
                .checked_sub(1)
                .and_then(|final_value| self.locate(from, final_value).ok())
                .map(|(bit, width)| bit + u64::from(width))
        } else {
            self.locate(from, next).ok().map(|(bit, _)| bit)
        };
        let held = 8 * (data.from + data.bytes.len()) as u64;
        match end_bit {
            Some(bit) if bit <= held => next.min(header.values).max(last + 1),
            _ => last + 1,
        }
    }

    /// Walk the packed values from `at` up to `end` (exclusive), calling
    /// `each` with the state before each value and the value undifferenced
    /// (`None` when missing), as `grib` decodes them. Checks the deadline
    /// as it goes.
    fn walk(
        &self,
        data: Data<'_>,
        mut at: Checkpoint,
        end: usize,
        mut each: impl FnMut(&Checkpoint, Option<i32>),
    ) -> Result<Checkpoint, PointError> {
        let layout = &self.header.layout;
        let descriptors = self.descriptors();
        let mut until_check = DEADLINE_STRIDE;
        while (at.packed as usize) < end {
            let index = at.group as usize;
            if index >= layout.groups {
                return Err(invalid("the groups end before the field"));
            }
            let group = descriptors.group(layout, index)?;
            if group.width > MAX_BITS {
                return Err(invalid("group width over 31 bits"));
            }
            while at.in_group < group.length && (at.packed as usize) < end {
                let raw = if group.width == 0 {
                    0
                } else {
                    let bit =
                        u64::from(at.group_bit) + u64::from(at.in_group) * u64::from(group.width);
                    data.read(bit, group.width)
                        .ok_or_else(|| invalid("data ends before the sampled values"))?
                };
                let before = at;
                let value = self
                    .unpack(&group, raw)
                    .map(|v| at.undifference(layout.order, &self.extra.first, v));
                each(&before, value);
                at.in_group += 1;
                at.packed += 1;
                until_check -= 1;
                if until_check == 0 {
                    until_check = DEADLINE_STRIDE;
                    ds_core::deadline::check().map_err(PointError::Fatal)?;
                }
            }
            if at.in_group >= group.length {
                let bits = u64::from(group.length) * u64::from(group.width);
                at.group_bit = u32::try_from(u64::from(at.group_bit) + bits)
                    .map_err(|_| invalid("data past 512 MiB"))?;
                at.group += 1;
                at.in_group = 0;
            }
        }
        Ok(at)
    }

    /// Group value `raw` (nothing for a constant group, of width 0) as
    /// `grib` unpacks it: plus the group's reference and the overall
    /// minimum; `None` for a missing value.
    fn unpack(&self, group: &Group, raw: u32) -> Option<i32> {
        let layout = &self.header.layout;
        let minimum = self.extra.minimum;
        if group.width == 0 {
            // A constant group: its reference, compared as `i32`.
            let missing1 = (1_i32 << layout.ref_bits).wrapping_sub(1);
            let missing2 = missing1.wrapping_sub(1);
            if layout.missing > 0 && group.reference == missing1
                || layout.missing == 2 && group.reference == missing2
            {
                return None;
            }
            Some(group.reference.wrapping_add(minimum))
        } else {
            let missing1 = (1_u32 << group.width) - 1;
            if layout.missing > 0 && raw == missing1 || layout.missing == 2 && raw == missing1 - 1 {
                return None;
            }
            // Under 2^31: `grib`'s sign-and-magnitude reading is the value.
            Some(
                (raw as i32)
                    .wrapping_add(group.reference)
                    .wrapping_add(minimum),
            )
        }
    }
}

/// The checkpoints a decode records: of row `next` on, at its first packed
/// value `start`.
struct Rows<'a> {
    found: Vec<Checkpoint>,
    next: usize,
    start: usize,
    ranks: Option<Ranks<'a>>,
    ni: usize,
    nj: usize,
}

impl Rows<'_> {
    fn row_start(&mut self) -> usize {
        let node = self.next * self.ni;
        match &mut self.ranks {
            Some(ranks) if self.next < self.nj => ranks.rank(node),
            _ => node,
        }
    }

    /// The decode stands at `at`: record it for every row starting there.
    /// Rows whose nodes the bitmap masks all start where the next does.
    fn pass(&mut self, at: &Checkpoint) {
        while self.next < self.nj && self.start == at.packed as usize {
            self.found.push(*at);
            self.next += 1;
            self.start = self.row_start();
        }
    }
}
