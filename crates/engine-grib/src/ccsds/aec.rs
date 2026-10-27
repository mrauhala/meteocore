//! The crate's only `unsafe`: a safe wrapper over the libaec decoder that
//! the `grib` crate links (`libaec-sys` 0.1.3, vendoring libaec 1.1.4), for
//! the two things `grib` does not expose (#1300): the bit offset at which
//! each reference sample interval (RSI) starts, recorded during a decode,
//! and a decode that starts at one of them.
//!
//! Every parameter, length and offset is checked here before libaec sees
//! it, and `avail_in` / `avail_out` bound libaec to the input and output
//! slices. Failures are [`AecError`] values, never panics.
//!
//! libaec's own `aec_decode_range` is not used: when the input ends before
//! the requested samples it copies them from an uninitialized buffer
//! without reporting it, and it leaks that buffer on errors. [`decode_rsi`]
//! does the same seek and decode on a caller-owned buffer and checks that
//! every requested byte was written.

use std::marker::PhantomData;

use libaec_sys as sys;

/// One CCSDS stream's coding parameters: GRIB2 template 5.42's bits per
/// value, block size, reference sample interval in blocks and compression
/// options mask (libaec's flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Params {
    pub bits_per_sample: u32,
    pub block_size: u32,
    pub rsi: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AecError {
    #[error("unsupported CCSDS parameters: {0}")]
    Params(&'static str),
    #[error("libaec {call} failed with status {status}")]
    Status { call: &'static str, status: i32 },
    #[error("CCSDS payload ended after {decoded} of {wanted} output bytes")]
    Short { decoded: usize, wanted: usize },
    #[error("CCSDS seek to bit {bit} is outside the {len}-byte input")]
    Seek { bit: usize, len: usize },
    #[error("CCSDS output of {0} bytes is not whole samples")]
    Output(usize),
}

impl Params {
    /// Whether libaec can decode these parameters, and an RSI's start can be
    /// sought: a block size of 8, 16, 32 or 64 samples (libaec's decoder does
    /// not check it, and a zero would loop), an RSI of 1 to 4096 blocks,
    /// 1 to 32 bits, and no `AEC_PAD_RSI`, whose recorded offsets point
    /// before the padding rather than at the next RSI.
    pub(crate) fn check(&self) -> Result<(), AecError> {
        if !(1..=32).contains(&self.bits_per_sample) {
            return Err(AecError::Params("bits per sample outside 1..=32"));
        }
        if !matches!(self.block_size, 8 | 16 | 32 | 64) {
            return Err(AecError::Params("block size not 8, 16, 32 or 64"));
        }
        if !(1..=4096).contains(&self.rsi) {
            return Err(AecError::Params(
                "reference sample interval outside 1..=4096",
            ));
        }
        if self.flags & sys::AEC_PAD_RSI != 0 {
            return Err(AecError::Params("padded RSIs"));
        }
        Ok(())
    }

    /// Bytes libaec writes per sample: 1, 2, 3 (up to 24 bits with
    /// `AEC_DATA_3BYTE`) or 4.
    pub(crate) fn bytes_per_sample(&self) -> usize {
        match self.bits_per_sample {
            0..=8 => 1,
            9..=16 => 2,
            17..=24 if self.flags & sys::AEC_DATA_3BYTE != 0 => 3,
            _ => 4,
        }
    }

    /// Samples in one reference sample interval.
    pub(crate) fn rsi_samples(&self) -> usize {
        self.rsi as usize * self.block_size as usize
    }

    pub(crate) fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }
}

/// libaec flags the CCSDS reader checks for.
pub(crate) const DATA_SIGNED: u32 = sys::AEC_DATA_SIGNED;
pub(crate) const DATA_MSB: u32 = sys::AEC_DATA_MSB;
pub(crate) const PAD_RSI: u32 = sys::AEC_PAD_RSI;

fn status(call: &'static str, status: i32) -> Result<(), AecError> {
    if status == sys::AEC_OK as i32 {
        Ok(())
    } else {
        Err(AecError::Status { call, status })
    }
}

/// An initialized libaec decode stream over `'a` input. Dropping it ends
/// the stream and frees libaec's state.
struct Decoder<'a> {
    stream: sys::aec_stream,
    bytes_per_sample: usize,
    input: PhantomData<&'a [u8]>,
}

impl<'a> Decoder<'a> {
    fn new(params: &Params, input: &'a [u8]) -> Result<Self, AecError> {
        params.check()?;
        // SAFETY: `aec_stream` is a C struct of integers and raw pointers,
        // for which all zeroes is valid: null pointers and no state, what
        // libaec expects before `aec_decode_init`.
        let mut stream: sys::aec_stream = unsafe { std::mem::zeroed() };
        stream.bits_per_sample = params.bits_per_sample;
        stream.block_size = params.block_size;
        stream.rsi = params.rsi;
        stream.flags = params.flags;
        stream.next_in = input.as_ptr();
        stream.avail_in = input.len();
        let mut decoder = Self {
            stream,
            bytes_per_sample: params.bytes_per_sample(),
            input: PhantomData,
        };
        // SAFETY: the parameters passed `check`, and `next_in`/`avail_in`
        // describe `input`, which outlives the decoder ('a). libaec stores
        // its allocated state in `stream.state` and keeps no pointer to the
        // stream itself, so the decoder may move. `Drop` frees that state,
        // also when the init fails after allocating it.
        let code = unsafe { sys::aec_decode_init(&mut decoder.stream) };
        status("aec_decode_init", code)?;
        Ok(decoder)
    }

    fn enable_offsets(&mut self) -> Result<(), AecError> {
        // SAFETY: an initialized stream that has decoded nothing yet.
        let code = unsafe { sys::aec_decode_enable_offsets(&mut self.stream) };
        status("aec_decode_enable_offsets", code)
    }

    /// Start at input bit `bit`, before decoding anything.
    fn seek(&mut self, bit: usize) -> Result<(), AecError> {
        // `aec_buffer_seek` skips `bit / 8` bytes and, for a bit inside a
        // byte, loads that byte: either way the byte must exist.
        if bit / 8 >= self.stream.avail_in {
            return Err(AecError::Seek {
                bit,
                len: self.stream.avail_in,
            });
        }
        // SAFETY: an initialized stream that has decoded nothing yet, and
        // byte `bit / 8` lies inside the input (checked above; libaec checks
        // `avail_in` again).
        let code = unsafe { sys::aec_buffer_seek(&mut self.stream, bit) };
        status("aec_buffer_seek", code)
    }

    /// Decode into `out` until it is full or the input ends, returning the
    /// bytes written.
    fn decode(&mut self, out: &mut [u8]) -> Result<usize, AecError> {
        if !out.len().is_multiple_of(self.bytes_per_sample) {
            return Err(AecError::Output(out.len()));
        }
        self.stream.next_out = out.as_mut_ptr();
        self.stream.avail_out = out.len();
        // SAFETY: an initialized stream. `next_in`/`avail_in` still describe
        // the unread rest of the input, which libaec advances together, and
        // `next_out`/`avail_out` describe `out`, live for this call; libaec
        // writes at most `avail_out` bytes. Its flag argument only matters
        // for encoding; `grib` passes `AEC_FLUSH`, and so does this.
        let code = unsafe { sys::aec_decode(&mut self.stream, sys::AEC_FLUSH as i32) };
        let written = out.len() - self.stream.avail_out;
        // Keep no pointer into `out` once the borrow ends.
        self.stream.next_out = std::ptr::null_mut();
        self.stream.avail_out = 0;
        status("aec_decode", code)?;
        Ok(written)
    }

    /// The bit offset of each RSI start the decode has reached, the first
    /// at 0 (needs [`Self::enable_offsets`]).
    fn offsets(&mut self) -> Result<Vec<usize>, AecError> {
        let mut count = 0usize;
        // SAFETY: an initialized stream; libaec writes one `size_t` (a
        // `usize` in the bindings) through the pointer.
        let code = unsafe { sys::aec_decode_count_offsets(&mut self.stream, &mut count) };
        status("aec_decode_count_offsets", code)?;
        let mut offsets = vec![0usize; count];
        // SAFETY: `offsets` holds `count` elements, the count libaec just
        // reported; it checks the length again before copying.
        let code = unsafe {
            sys::aec_decode_get_offsets(&mut self.stream, offsets.as_mut_ptr(), offsets.len())
        };
        status("aec_decode_get_offsets", code)?;
        Ok(offsets)
    }
}

impl Drop for Decoder<'_> {
    fn drop(&mut self) {
        if !self.stream.state.is_null() {
            // SAFETY: `state` is non-null only after `aec_decode_init`
            // allocated it, and is ended exactly once, here. Its status is
            // always OK.
            unsafe { sys::aec_decode_end(&mut self.stream) };
        }
    }
}

/// Decode the first `samples` samples of `payload`, `chunk` at a time
/// through one reused buffer, never the whole field, handing `sink` each
/// chunk's raw bytes with the index of its first sample; then return the
/// bit offset at which each RSI the decode reached starts. Output that
/// ends before `samples` is [`AecError::Short`].
pub(crate) fn decode_indexed<E: From<AecError>>(
    params: &Params,
    payload: &[u8],
    samples: usize,
    chunk: usize,
    sink: impl FnMut(usize, &[u8]) -> Result<(), E>,
) -> Result<Vec<usize>, E> {
    let decoded = decode_from(params, payload, 0, samples, chunk, sink)?;
    if decoded.samples != samples {
        let width = params.bytes_per_sample();
        return Err(AecError::Short {
            decoded: decoded.samples * width,
            wanted: samples.saturating_mul(width),
        }
        .into());
    }
    Ok(decoded.offsets)
}

/// What [`decode_from`] decoded.
#[derive(Debug)]
pub(crate) struct Decoded {
    /// Samples produced, from the RSI the decode started at.
    pub samples: usize,
    /// The bit of the input at which each RSI the decode reached starts:
    /// first the one it started at, then each the decode entered.
    pub offsets: Vec<usize>,
}

/// Decode up to `samples` samples from the RSI whose first bit is `bit` of
/// `input`, `chunk` at a time through one reused buffer, handing `sink`
/// each chunk with the index of its first sample, counted from that RSI's
/// first; and record where each RSI the decode enters starts. An input that
/// ends first is no error: a prefix of a payload, or the bytes from one RSI
/// on, decode as far as they reach (#1301).
pub(crate) fn decode_from<E: From<AecError>>(
    params: &Params,
    input: &[u8],
    bit: usize,
    samples: usize,
    chunk: usize,
    mut sink: impl FnMut(usize, &[u8]) -> Result<(), E>,
) -> Result<Decoded, E> {
    let mut decoder = Decoder::new(params, input)?;
    decoder.seek(bit)?;
    decoder.enable_offsets()?;
    let width = decoder.bytes_per_sample;
    let chunk = chunk.clamp(1, samples.max(1));
    let mut buffer = vec![
        0;
        chunk
            .checked_mul(width)
            .ok_or(AecError::Output(usize::MAX))?
    ];
    let mut first = 0;
    while first < samples {
        let out = &mut buffer[..chunk.min(samples - first) * width];
        let wanted = out.len();
        let written = decoder.decode(out)?;
        if written > 0 {
            sink(first, &out[..written])?;
        }
        first += written / width;
        if written < wanted {
            break; // the input ended
        }
    }
    let mut offsets = decoder.offsets()?;
    // libaec counts from where it started reading: past the sought byte when
    // `bit` falls inside one (`aec_buffer_seek` loads that byte), and in
    // `size_t`, so a position inside that byte wraps. Its first entry is
    // the placeholder `aec_decode_enable_offsets` pushes: the RSI at `bit`.
    let base = 8 * (bit / 8) + if bit.is_multiple_of(8) { 0 } else { 8 };
    for offset in offsets.iter_mut().skip(1) {
        *offset = base.wrapping_add(*offset);
    }
    if let Some(start) = offsets.first_mut() {
        *start = bit;
    }
    Ok(Decoded {
        samples: first,
        offsets,
    })
}

/// Decode `out.len()` bytes of samples from the RSI whose first bit is
/// `bit` of `input`: the whole payload, or (#1301) bytes fetched from that
/// RSI's first byte on. `out` must be whole samples, and every byte of it
/// must decode, else [`AecError::Short`].
pub(crate) fn decode_rsi(
    params: &Params,
    input: &[u8],
    bit: usize,
    out: &mut [u8],
) -> Result<(), AecError> {
    let mut decoder = Decoder::new(params, input)?;
    decoder.seek(bit)?;
    let written = decoder.decode(out)?;
    if written == out.len() {
        Ok(())
    } else {
        Err(AecError::Short {
            decoded: written,
            wanted: out.len(),
        })
    }
}

/// CCSDS-encode `samples` (big-endian `bytes_per_sample` each), for test
/// fixtures. Panics on failure.
#[cfg(test)]
pub(crate) fn encode(params: &Params, samples: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; samples.len() * 2 + 4096];
    // SAFETY: as in `Decoder::new`: zeroed is the stream's initial state.
    let mut stream: sys::aec_stream = unsafe { std::mem::zeroed() };
    stream.bits_per_sample = params.bits_per_sample;
    stream.block_size = params.block_size;
    stream.rsi = params.rsi;
    stream.flags = params.flags;
    stream.next_in = samples.as_ptr();
    stream.avail_in = samples.len();
    stream.next_out = out.as_mut_ptr();
    stream.avail_out = out.len();
    // SAFETY: the stream describes `samples` and `out`, both live across
    // these calls; `aec_encode_end` frees the state `aec_encode_init` made.
    let written = unsafe {
        assert_eq!(sys::aec_encode_init(&mut stream), sys::AEC_OK as i32);
        assert_eq!(
            sys::aec_encode(&mut stream, sys::AEC_FLUSH as i32),
            sys::AEC_OK as i32
        );
        let written = stream.total_out;
        sys::aec_encode_end(&mut stream);
        written
    };
    out.truncate(written);
    out
}
