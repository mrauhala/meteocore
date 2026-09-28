//! Preflight Blosc frames without replacing its block-level partial decoding.
use super::*;
use zarrs::{
    array::codec::bytes_to_bytes::blosc::{blosc_decompress_bytes_partial, blosc_validate},
    storage::{
        byte_range::{ByteRange, ByteRangeIterator},
        StorageError,
    },
};

#[cfg(feature = "icechunk")]
use zarrs::array::codec::api::AsyncBytesPartialDecoderTraits;

mod partial;
pub(super) use partial::PartialDecoder;

pub(super) struct Frame {
    length: u64,
    typesize: u64,
    _scratch: Option<crate::encoded::Scratch>,
}

impl Frame {
    // c-blosc 1 getitem returns before freeing scratch on out-of-bounds
    // regions. Resolve every range against the checked frame before getitem.
    fn bounds(&self, range: ByteRange) -> Option<(usize, usize)> {
        let (start, end) = match range {
            ByteRange::FromStart(start, Some(length)) => (start, start.checked_add(length)?),
            ByteRange::FromStart(start, None) => (start, self.length),
            ByteRange::Suffix(length) => (self.length.checked_sub(length)?, self.length),
        };
        // Checked frame lengths fit c-blosc's 32-bit buffer sizes.
        (start <= end && end <= self.length).then_some((start as usize, end as usize))
    }
}

pub(super) fn validate_and_admit(
    bytes: &[u8],
    representation: &BytesRepresentation,
    partial: bool,
    context: Option<&Arc<crate::encoded::Context>>,
) -> Result<Frame, CodecError> {
    check_deadline()?;
    let limit = representation.size().ok_or_else(|| {
        CodecError::Other("Blosc decoding requires a bounded representation".into())
    })?;
    // Upstream validation checks the 16-byte header, format version, encoded
    // length and maximum output size without allocating a decode destination.
    let length =
        blosc_validate(bytes).ok_or_else(|| CodecError::Other("invalid Blosc frame".into()))?;
    if length as u64 > limit
        || (matches!(representation, BytesRepresentation::FixedSize(_)) && length as u64 != limit)
    {
        return Err(CodecError::Other(
            "Blosc output does not match its declared representation".into(),
        ));
    }
    // c-blosc 1's header stores a little-endian block size at bytes 8..12.
    // Bound it before getitem/full decoding can allocate native scratch.
    let block = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as u64;
    let typesize = u64::from(bytes[3]);
    // c-blosc's BLOSC_MAX_BLOCKSIZE ensures 3*block + 4*typesize fits int32.
    const MAX_BLOCK: u64 = (i32::MAX as u64 - 4 * 255) / 3;
    if block == 0 || block > length as u64 || block > MAX_BLOCK || typesize == 0 {
        return Err(CodecError::Other("invalid Blosc block/type size".into()));
    }
    let scratch = if let Some(context) = context {
        // serial_blosc uses 2*block + 4*typesize; getitem uses 3*block +
        // 4*typesize, independently of how few values the request selects.
        let scratch = block * if partial { 3 } else { 2 } + 4 * typesize;
        let scratch = context.codec_scratch(scratch).map_err(io_error)?;
        if matches!(representation, BytesRepresentation::BoundedSize(_)) {
            context.intermediate(length).map_err(io_error)?;
        }
        Some(scratch)
    } else {
        None
    };
    Ok(Frame {
        length: length as u64,
        typesize,
        _scratch: scratch,
    })
}
