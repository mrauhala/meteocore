//! The CCSDS point reader against the full decode, bit for bit: the
//! committed ECMWF field (0.25°, `lon_first` 180°, 254 RSIs), synthetic
//! fields over every sample width, and malformed input (#1300).

use std::time::Instant;

use super::aec::{self, AecError, Params};
use super::*;
use crate::reader::decode_message;
use crate::test_support::Ccsds;

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/grib-local/sample-message.grib2"
    ))
    .unwrap()
}

fn bits(values: &[Option<f64>]) -> Vec<Option<u64>> {
    values.iter().map(|v| v.map(f64::to_bits)).collect()
}

/// `coords` sampled from the full decode of `message`.
fn decoded(message: &[u8], coords: &[(f64, f64)]) -> Vec<Option<f64>> {
    let grid = decode_message(message, "q").unwrap();
    coords
        .iter()
        .map(|&(lon, lat)| grid.bilinear_value(lon, lat))
        .collect()
}

/// Sample `coords` while indexing, through the index as one batch and one
/// point at a time, each bit for bit the full decode's.
fn assert_identical(message: &[u8], coords: &[(f64, f64)]) -> CcsdsIndex {
    let expected = bits(&decoded(message, coords));
    let (index, cold) = Header::parse(message)
        .unwrap()
        .index(message, coords)
        .unwrap();
    assert_eq!(bits(&cold), expected, "while indexing");
    assert_eq!(
        bits(&index.sample(message, coords).unwrap()),
        expected,
        "batch"
    );
    for (&coord, &expected) in coords.iter().zip(&expected) {
        let single = index.sample(message, &[coord]).unwrap();
        assert_eq!(single[0].map(f64::to_bits), expected, "{coord:?}");
    }
    index
}

/// Seams, poles, the grid's own first node and points just off it, on a
/// grid sweep.
fn edge_points() -> Vec<(f64, f64)> {
    let mut coords = Vec::new();
    for lat in [90.0, 89.9, 60.3, 0.0, -0.1, -33.9, -89.9, -90.0] {
        for lon in [
            -180.0, -179.9, -0.1, 0.0, 0.1, 24.96, 179.9, 179.99, 180.0, 180.1, 359.9, 360.0,
        ] {
            coords.push((lon, lat));
        }
    }
    // Off the grid, and not a coordinate at all.
    coords.extend([
        (10.0, 90.1),
        (10.0, -90.1),
        (f64::NAN, 0.0),
        (0.0, f64::INFINITY),
    ]);
    for i in 0..97 {
        coords.push((-180.0 + i as f64 * 5.39, 90.0 - i as f64 * 1.87));
    }
    coords
}

/// Points whose bilinear neighbours straddle the start of RSI `rsi`, of
/// `per_rsi` samples, on `lattice`: one either side of that node's column
/// and row.
fn straddling(lattice: &Lattice, per_rsi: usize, rsi: usize) -> Vec<(f64, f64)> {
    let node = rsi * per_rsi;
    let (row, col) = ((node / lattice.ni) as f64, (node % lattice.ni) as f64);
    [(-0.5, 0.5), (-0.5, -0.5), (0.5, -0.5), (0.25, 0.75)]
        .iter()
        .map(|(dc, dr)| {
            (
                lattice.lon_first + (col + dc) * lattice.lon_inc,
                lattice.lat_first + (row + dr) * lattice.lat_inc,
            )
        })
        .collect()
}

/// RSI `rsi` decoded whole through the index and scaled.
fn rsi_values(index: &CcsdsIndex, message: &[u8], rsi: usize) -> Vec<f32> {
    let header = &index.header;
    let params = header.packing.aec;
    let per_rsi = params.rsi_samples();
    let count = per_rsi.min(header.packing.values - rsi * per_rsi);
    let width = params.bytes_per_sample();
    let mut out = vec![0; count * width];
    let payload = header.payload(message).unwrap();
    aec::decode_rsi(&params, payload, index.rsi_offsets[rsi] as usize, &mut out).unwrap();
    (0..count)
        .map(|k| header.packing.value(packed(&out, k, width)))
        .collect()
}

fn f32_bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|v| v.to_bits()).collect()
}

#[test]
fn the_fixture_is_ecmwf_ccsds_and_indexes_in_about_a_kilobyte_and_a_quarter() {
    let message = fixture();
    let header = Header::parse(&message).unwrap();
    assert_eq!(
        header.lattice,
        Lattice {
            ni: 1440,
            nj: 721,
            lon_first: 180.0,
            lat_first: 90.0,
            lon_inc: 0.25,
            lat_inc: -0.25,
        }
    );
    assert_eq!(header.packing.aec.block_size, 32);
    assert_eq!(header.packing.aec.rsi, 128);
    assert_eq!(header.packing.aec.flags, 14);
    assert_eq!(header.packing.values, 1440 * 721);
    assert_eq!(header.constant, None);
    let (index, _) = header.index(&message, &[]).unwrap();
    assert_eq!(index.rsi_offsets.len(), (1440 * 721usize).div_ceil(4096));
    assert_eq!(index.rsi_offsets.len(), 254);
    // 254 offsets of 4 bytes and the header.
    assert!(index.size_bytes() < 1300, "{}", index.size_bytes());
}

/// What the point path records in the parameter and discovery caches is
/// what a full decode records: the WMO triple, centre, surface, statistic,
/// geometry and u/v frame.
fn assert_full_decode_metadata(message: &[u8]) {
    let header = Header::parse(message).unwrap();
    let decoded = MessageMetadata::from(&decode_message(message, "q").unwrap());
    assert_eq!(header.metadata, decoded);
    assert!(header.metadata.geometry.is_some());
    assert_eq!(header.metadata.geometry, decoded.geometry);
    assert_eq!(header.metadata.uv_frame, decoded.uv_frame);
    let (index, _) = header.index(message, &[]).unwrap();
    assert_eq!(*index.metadata(), decoded);
}

#[test]
fn header_metadata_is_the_full_decode_metadata() {
    let message = fixture();
    assert_full_decode_metadata(&message);
    assert_eq!(
        Header::parse(&message).unwrap().metadata.uv_frame,
        ds_core::wind::VectorFrame::Earth
    );
    // Grid-relative components, and a grid from 0°E.
    let raw = vec![7; 360 * 181];
    let mut grid_relative = Ccsds::global().message(&raw);
    // Section 3 starts at 37; its resolution and component flags are at 54.
    grid_relative[37 + 54] |= 0x08;
    assert_full_decode_metadata(&grid_relative);
    assert_eq!(
        Header::parse(&grid_relative).unwrap().metadata.uv_frame,
        ds_core::wind::VectorFrame::Grid
    );
}

#[test]
fn fixture_points_are_bit_identical_to_the_full_decode() {
    let message = fixture();
    assert_identical(&message, &edge_points());
}

#[test]
fn points_straddling_rsi_boundaries_and_in_the_last_rsi() {
    let message = fixture();
    let header = Header::parse(&message).unwrap();
    let lattice = header.lattice;
    let mut coords = Vec::new();
    for rsi in [1, 2, 100, 252, 253] {
        let points = straddling(&lattice, 4096, rsi);
        // Each set really reads two RSIs.
        let rsis: std::collections::BTreeSet<_> = points
            .iter()
            .filter_map(|&(lon, lat)| lattice.bilinear(lon, lat))
            .flat_map(|b| b.nodes.map(|node| node / 4096))
            .collect();
        assert_eq!(rsis.len(), 2, "RSI {rsi}: {rsis:?}");
        coords.extend(points);
    }
    // The last RSI holds the last 1952 nodes: the southernmost rows.
    for lon in [-180.0, -60.0, 0.0, 33.3, 179.9] {
        coords.extend([(lon, -89.9), (lon, -90.0), (lon, -89.6)]);
    }
    assert_identical(&message, &coords);
}

#[test]
fn whole_rsis_decode_to_the_full_decode_values() {
    let message = fixture();
    let grid = decode_message(&message, "q").unwrap();
    let (index, _) = Header::parse(&message)
        .unwrap()
        .index(&message, &[])
        .unwrap();
    for rsi in [0, 1, 127, 252, 253] {
        let start = rsi * 4096;
        let values = rsi_values(&index, &message, rsi);
        let expected = &grid.values[start..(start + 4096).min(grid.values.len())];
        assert_eq!(f32_bits(&values), f32_bits(expected), "RSI {rsi}");
    }
}

/// The streamed index decode yields the same bytes and offsets whatever
/// its chunk, and every scaled sample is the full decode's.
#[test]
fn chunked_index_decode_is_the_full_decode() {
    let message = fixture();
    let header = Header::parse(&message).unwrap();
    let grid = decode_message(&message, "q").unwrap();
    let params = header.packing.aec;
    let payload = header.payload(&message).unwrap();
    let values = header.packing.values;
    let width = params.bytes_per_sample();
    let decode = |chunk: usize| {
        let mut bytes = Vec::with_capacity(values * width);
        let offsets = aec::decode_indexed(&params, payload, values, chunk, |first, out| {
            assert_eq!(first * width, bytes.len());
            bytes.extend_from_slice(out);
            Ok::<_, AecError>(())
        })
        .unwrap();
        (bytes, offsets)
    };
    let whole = decode(values);
    let scaled: Vec<f32> = (0..values)
        .map(|k| header.packing.value(packed(&whole.0, k, width)))
        .collect();
    assert_eq!(f32_bits(&scaled), f32_bits(&grid.values));
    for chunk in [7, 4096, 5000, CHUNK_SAMPLES] {
        assert!(decode(chunk) == whole, "chunk {chunk}");
    }
}

/// Every sample width `grib` and libaec agree on, both grid origins, many
/// small RSIs, scale factors either way and a zero-bit field.
#[test]
fn synthetic_layouts_are_bit_identical() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for (bits, flags, block, rsi, binary, decimal) in [
        (16, 14, 32, 128, -6, 0),
        (8, 12, 8, 3, 0, 1),
        (12, 14, 16, 5, -10, 2),
        (24, 14, 32, 7, -2, -1),
        (32, 14, 64, 2, 3, 0),
        (5, 8, 8, 1, 0, 0),
        (0, 14, 32, 128, 0, 2),
    ] {
        for lon_first in [0, 180_000_000] {
            let spec = Ccsds {
                lon_first,
                bits,
                flags,
                block,
                rsi,
                binary,
                decimal,
                reference: -37.25,
                ..Ccsds::global()
            };
            let mask = if bits == 32 {
                u32::MAX
            } else {
                (1u32 << bits) - 1
            };
            let raw: Vec<u32> = (0..spec.ni * spec.nj)
                .map(|_| (next() as u32) & mask)
                .collect();
            let message = spec.message(&raw);
            let header = Header::parse(&message).unwrap();
            let lattice = header.lattice;
            let mut coords = edge_points();
            if bits > 0 {
                let per_rsi = header.packing.aec.rsi_samples();
                let rsis = header.packing.values.div_ceil(per_rsi);
                for rsi in [1, rsis / 2, rsis - 1] {
                    coords.extend(straddling(&lattice, per_rsi, rsi));
                }
            }
            let index = assert_identical(&message, &coords);
            assert_full_decode_metadata(&message);
            for coords in [&coords[..], &[(24.0, 60.0)], &[(150.0, -34.0)]] {
                assert_partial_reads(&message, coords);
            }
            if bits > 0 {
                let grid = decode_message(&message, "q").unwrap();
                let per_rsi = header.packing.aec.rsi_samples();
                let last = index.rsi_offsets.len() - 1;
                for rsi in [0, 1, last] {
                    let start = rsi * per_rsi;
                    let end = (start + per_rsi).min(grid.values.len());
                    assert_eq!(
                        f32_bits(&rsi_values(&index, &message, rsi)),
                        f32_bits(&grid.values[start..end]),
                        "{bits} bits, RSI {rsi}"
                    );
                }
            } else {
                assert!(index.rsi_offsets.is_empty());
            }
        }
    }
}

#[test]
fn other_layouts_are_unsupported() {
    let raw = vec![7; 360 * 181];
    let unsupported = |spec: Ccsds| {
        let message = spec.message(&raw);
        match Header::parse(&message) {
            Err(PointError::Unsupported(reason)) => reason,
            other => panic!("{spec:?}: {other:?}"),
        }
    };
    let global = Ccsds::global;
    assert_eq!(
        unsupported(Ccsds {
            bitmap: 0,
            ..global()
        }),
        "bitmap"
    );
    assert_eq!(
        unsupported(Ccsds {
            scan: 64,
            ..global()
        }),
        "scan mode"
    );
    // 16-bit samples written least significant byte first.
    assert_eq!(
        unsupported(Ccsds {
            flags: 8,
            ..global()
        }),
        "least significant byte first"
    );
    assert_eq!(
        unsupported(Ccsds {
            flags: 15,
            ..global()
        }),
        "signed samples"
    );
    assert_eq!(
        unsupported(Ccsds {
            flags: 14 | 32,
            ..global()
        }),
        "padded RSIs"
    );
    // 24-bit samples libaec writes in four bytes, `grib` reads in three.
    assert_eq!(
        unsupported(Ccsds {
            bits: 24,
            flags: 12,
            ..global()
        }),
        "sample width"
    );
    assert_eq!(
        unsupported(Ccsds {
            binary: 127,
            ..global()
        }),
        "binary scale factor"
    );
    // Simple packing.
    let simple = crate::test_support::message(0, 280.0, [0, 2, 4, 6], 103, 2);
    assert!(matches!(
        Header::parse(&simple),
        Err(PointError::Unsupported("not CCSDS packing"))
    ));
}

/// Odd bit widths, block sizes and intervals never reach libaec.
#[test]
fn invalid_coding_parameters_are_errors() {
    let raw = vec![7; 360 * 181];
    let mut message = Ccsds::global().message(&raw);
    // Section 5 starts at 143: bits at octet 20, block 23, RSI 24-25.
    let s5 = 143;
    assert_eq!(message[s5 + 4], 5);
    for (offset, value) in [(22, 0u8), (22, 12), (23, 0), (19, 33)] {
        let saved = message[s5 + offset];
        message[s5 + offset] = value;
        if offset == 23 {
            message[s5 + 24] = 0;
        }
        let result = Header::parse(&message);
        assert!(
            matches!(
                result,
                Err(PointError::Invalid(_) | PointError::Unsupported(_))
            ),
            "octet {offset} = {value}: {result:?}"
        );
        message[s5 + offset] = saved;
        message[s5 + 24] = 128;
    }
    Header::parse(&message).unwrap();
    for params in [
        Params {
            bits_per_sample: 0,
            block_size: 32,
            rsi: 128,
            flags: 14,
        },
        Params {
            bits_per_sample: 33,
            block_size: 32,
            rsi: 128,
            flags: 14,
        },
        Params {
            bits_per_sample: 16,
            block_size: 0,
            rsi: 128,
            flags: 14,
        },
        Params {
            bits_per_sample: 16,
            block_size: 32,
            rsi: 0,
            flags: 14,
        },
        Params {
            bits_per_sample: 16,
            block_size: 32,
            rsi: 4097,
            flags: 14,
        },
    ] {
        assert!(matches!(params.check(), Err(AecError::Params(_))));
        let mut out = [0; 8];
        assert!(aec::decode_rsi(&params, &[0; 64], 0, &mut out).is_err());
        let indexed = aec::decode_indexed(&params, &[0; 64], 4, 4, |_, _| Ok::<_, AecError>(()));
        assert!(indexed.is_err());
    }
}

/// A message cut short, a payload that ends early, corrupt payload bytes
/// and an index that does not match: errors or, where libaec decodes
/// garbage without noticing, exactly the full decode's garbage. Never a
/// panic, never a value the full decode would not give.
#[test]
fn malformed_messages_fail_or_agree_with_the_full_decode() {
    let message = fixture();
    let coords = edge_points();
    let header = Header::parse(&message).unwrap();
    let start = header.payload_start;

    // Truncated messages: unparseable, or a short payload.
    for cut in [10, 100, message.len() / 2, message.len() - 5] {
        let short = &message[..message.len() - cut];
        let result = Header::parse(short).and_then(|header| header.index(short, &coords));
        assert!(result.is_err(), "cut {cut}");
    }

    // Section 7 declaring fewer bytes: the full decode zero-fills the tail
    // without an error, the index refuses.
    let mut truncated = message[..message.len() - 4 - 1000].to_vec();
    truncated.extend(b"7777");
    let s7 = start - 5;
    let s7_len = u32::from_be_bytes(truncated[s7..s7 + 4].try_into().unwrap()) - 1000;
    truncated[s7..s7 + 4].copy_from_slice(&s7_len.to_be_bytes());
    let total = truncated.len() as u64;
    truncated[8..16].copy_from_slice(&total.to_be_bytes());
    assert!(decode_message(&truncated, "q").is_ok());
    assert!(matches!(
        Header::parse(&truncated)
            .unwrap()
            .index(&truncated, &coords),
        Err(PointError::Invalid(_))
    ));
    // Through the points only (#1337): a point the missing tail holds fails,
    // one before it is the full decode's.
    let cut = Header::parse(&truncated).unwrap();
    assert!(matches!(
        cut.index_to(None, &truncated, &coords),
        Err(PointError::Invalid(_))
    ));
    let north = [(24.96, 60.32)];
    let (_, values) = cut.index_to(None, &truncated, &north).unwrap();
    assert_eq!(bits(&values), bits(&decoded(&truncated, &north)));
    assert!(matches!(
        cut.index_to(None, &message, &north),
        Err(PointError::Invalid(_))
    ));

    // Corrupt payload bytes.
    for (at, len, byte) in [(0, 64, 0xff), (5000, 4096, 0), (100_000, 8, 0x55)] {
        let mut corrupt = message.clone();
        let at = start + at.min(header.payload_len - len);
        corrupt[at..at + len].fill(byte);
        let header = Header::parse(&corrupt).unwrap();
        match header.index(&corrupt, &coords) {
            Ok((index, values)) => {
                let expected = bits(&decoded(&corrupt, &coords));
                assert_eq!(bits(&values), expected);
                assert_eq!(bits(&index.sample(&corrupt, &coords).unwrap()), expected);
                let (_, values) = header.index_to(None, &corrupt, &coords).unwrap();
                assert_eq!(bits(&values), expected, "through the points");
            }
            // Through the points only, the decode may stop before what the
            // complete index refused: no panic, no other error.
            Err(PointError::Invalid(_)) => assert!(matches!(
                header.index_to(None, &corrupt, &coords),
                Ok(_) | Err(PointError::Invalid(_))
            )),
            Err(other) => panic!("{other:?}"),
        }
    }

    // An index that does not match the message.
    let (index, _) = header.index(&message, &[]).unwrap();
    let south = [(0.0, -89.9)];
    let partial = CcsdsIndex {
        header: index.header,
        rsi_offsets: index.rsi_offsets[..10].into(),
    };
    assert!(partial.sample(&message, &[(0.0, 89.9)]).is_ok());
    assert!(matches!(
        partial.sample(&message, &south),
        Err(PointError::Invalid(_))
    ));
    let mut past = index.rsi_offsets.to_vec();
    *past.last_mut().unwrap() = u32::MAX;
    let past = CcsdsIndex {
        header: index.header,
        rsi_offsets: past.into(),
    };
    assert!(matches!(
        past.sample(&message, &south),
        Err(PointError::Invalid(_))
    ));
    let mut longer = message.clone();
    longer.push(0);
    assert!(matches!(
        index.sample(&longer, &south),
        Err(PointError::Invalid(_))
    ));
    // Offset tables libaec could not have produced.
    let offsets: Vec<usize> = index.rsi_offsets.iter().map(|&o| o as usize).collect();
    assert!(header.rsi_offsets(&offsets, true).is_ok());
    assert!(header.rsi_offsets(&offsets[..253], true).is_err());
    // A prefix's first RSIs are a partial index, but never none.
    assert!(header.rsi_offsets(&offsets[..253], false).is_ok());
    assert!(header.rsi_offsets(&[], false).is_err());
    let mut swapped = offsets.clone();
    swapped.swap(3, 4);
    assert!(header.rsi_offsets(&swapped, false).is_err());
    let mut late = offsets.clone();
    late[0] = 1;
    assert!(header.rsi_offsets(&late, false).is_err());
    let small = Header {
        payload_len: 1000,
        ..header
    };
    assert!(small.rsi_offsets(&offsets, true).is_err());
}

#[test]
fn rsi_decodes_check_their_input_and_output() {
    let message = fixture();
    let header = Header::parse(&message).unwrap();
    let params = header.packing.aec;
    let payload = header.payload(&message).unwrap();
    let mut out = vec![0; 2 * params.bytes_per_sample()];
    assert!(matches!(
        aec::decode_rsi(&params, payload, payload.len() * 8, &mut out),
        Err(AecError::Seek { .. })
    ));
    assert!(matches!(
        aec::decode_rsi(&params, &[], 0, &mut out),
        Err(AecError::Seek { .. })
    ));
    assert!(matches!(
        aec::decode_rsi(&params, payload, 0, &mut out[..3]),
        Err(AecError::Output(3))
    ));
    assert!(matches!(
        aec::decode_rsi(&params, &payload[..8], 0, &mut [0; 8192]),
        Err(AecError::Short { .. })
    ));
    aec::decode_rsi(&params, payload, 0, &mut out).unwrap();
}

/// What the partial reads of #1301 did on one message, for the summary the
/// ignored local-file test prints.
#[derive(Debug, Default)]
struct PartialReads {
    /// Bytes the needed RSIs span, and the message bytes a prefix through
    /// them holds.
    span: usize,
    prefix: usize,
    /// Whether a read on from a partial index began mid-byte, and whether
    /// a short prefix needed its continuation.
    mid_byte: bool,
    continued: bool,
}

/// Every partial read of `message` for `coords` gives the full decode's
/// values, bit for bit, and indexes exactly the RSI starts a whole decode
/// records:
/// - warm: the one span of the needed RSIs [`CcsdsIndex::rsi_bytes`] names;
/// - cold: a prefix through them, parsed without the rest of the message;
/// - cold and short: a prefix ending halfway into them, then one
///   continuation to the read-on its partial index predicts;
/// - extended: a partial index read on from where it ends, over only the
///   bytes [`CcsdsIndex::extension`] names, one continuation if short;
/// - whole (#1337): the whole message indexed only through the needed
///   RSIs, and an index of the first RSI's points extended to them and on
///   to the complete index, from the same bytes.
fn assert_partial_reads(message: &[u8], coords: &[(f64, f64)]) -> PartialReads {
    let expected = bits(&decoded(message, coords));
    let len = message.len();
    let header = Header::parse(message).unwrap();
    let (complete, _) = header.index(message, &[]).unwrap();
    assert!(complete.complete());
    let exact = |index: &CcsdsIndex| {
        assert_eq!(
            index.rsi_offsets[..],
            complete.rsi_offsets[..index.rsi_offsets.len()],
            "RSI starts"
        );
    };
    let mut reads = PartialReads::default();
    let Some(needs) = complete.needs(coords) else {
        // A constant field, or no point on the grid: the header suffices.
        let prefix = &message[..MIN_PREFIX.min(len)];
        let header = Header::parse_prefix(prefix, len).unwrap().unwrap();
        let Extended::Done { values, .. } = header.extend(None, prefix, 0, coords).unwrap() else {
            panic!("nothing to decode")
        };
        assert_eq!(bits(&values), expected);
        let (index, values) = header.index_to(None, message, coords).unwrap();
        assert_eq!(bits(&values), expected, "whole");
        assert!(index.reaches(coords));
        assert_eq!(index.complete(), header.constant.is_some());
        return reads;
    };
    assert_whole_reads(message, &complete, coords, &expected);
    let span = complete.rsi_bytes(&needs).unwrap();
    reads.span = span.len();
    reads.prefix = span.end;
    let warm = complete
        .sample_bytes(&message[span.clone()], span.start, coords)
        .unwrap();
    assert_eq!(bits(&warm), expected, "warm range");
    // Bytes that begin after the first needed RSI does.
    assert!(matches!(
        complete.sample_bytes(&message[span.start + 1..span.end], span.start + 1, coords),
        Err(PointError::Invalid(_))
    ));

    // Cold, through the needed RSIs.
    let prefix = &message[..span.end];
    let parsed = Header::parse_prefix(prefix, len).unwrap().unwrap();
    assert_eq!(parsed, header);
    let Extended::Done { index, values } = parsed.extend(None, prefix, 0, coords).unwrap() else {
        panic!("a prefix through the needed RSIs is enough")
    };
    assert_eq!(bits(&values), expected, "prefix");
    assert!(index.covers(&needs));
    exact(&index);

    // Cold and short: one continuation.
    let short = ((span.start + span.end) / 2).max(header.payload_start + 1);
    let read = |prior: Option<&CcsdsIndex>, bytes: &[u8], start: usize| {
        header.extend(prior, bytes, start, coords).unwrap()
    };
    let partial = match read(None, &message[..short], 0) {
        // The needed samples lay in the first half after all.
        Extended::Done { index, values } => {
            assert_eq!(bits(&values), expected, "half a prefix");
            exact(&index);
            index
        }
        Extended::Short { index: partial } => {
            exact(&partial);
            let until = partial.read_on(&needs, None).end;
            assert!(until > short && until <= len);
            reads.continued = true;
            match read(Some(&partial), &message[..until], 0) {
                Extended::Done { index, values } => {
                    exact(&index);
                    assert_eq!(bits(&values), expected, "continued prefix");
                }
                Extended::Short { .. } => panic!("the continuation's prediction fell short"),
            }
            partial
        }
    };

    // Extended from the short prefix's partial index.
    let range = partial.read_on(&needs, None);
    let from_rsi = needs.first_rsi.min(partial.rsi_offsets.len() - 1);
    reads.mid_byte = partial.rsi_offsets[from_rsi] % 8 != 0;
    assert_eq!(
        range.start,
        header.payload_start + partial.rsi_offsets[from_rsi] as usize / 8
    );
    let values = match read(Some(&partial), &message[range.clone()], range.start) {
        Extended::Done { index, values } => {
            exact(&index);
            values
        }
        Extended::Short { index } => {
            exact(&index);
            let until = index.read_on(&needs, None).end;
            let Extended::Done { index, values } =
                read(Some(&index), &message[range.start..until], range.start)
            else {
                panic!("the extension's continuation fell short")
            };
            exact(&index);
            values
        }
    };
    assert_eq!(bits(&values), expected, "extended");
    reads
}

/// The whole message indexed only through the RSIs `coords` need (#1337),
/// bit for bit `expected`, recording exactly the RSI starts `complete`
/// does: from nothing, stopping at the start of the RSI after the last
/// needed one; on from an index of the first row's points; then on to the
/// last row, which completes the index.
fn assert_whole_reads(
    message: &[u8],
    complete: &CcsdsIndex,
    coords: &[(f64, f64)],
    expected: &[Option<u64>],
) {
    let header = complete.header;
    let lattice = header.lattice;
    let exact = |index: &CcsdsIndex| {
        assert_eq!(
            index.rsi_offsets[..],
            complete.rsi_offsets[..index.rsi_offsets.len()],
            "RSI starts"
        );
    };
    let needs = complete.needs(coords).unwrap();
    let (through, values) = header.index_to(None, message, coords).unwrap();
    assert_eq!(bits(&values), expected, "whole, through the points");
    exact(&through);
    assert!(through.covers(&needs) && through.reaches(coords));
    assert!(through.rsi_offsets.len() <= needs.last_rsi + 2);
    assert_eq!(
        bits(&through.sample(message, coords).unwrap()),
        expected,
        "through the partial index"
    );

    let first = [(lattice.lon_first, lattice.lat_first)];
    let (north, _) = header.index_to(None, message, &first).unwrap();
    exact(&north);
    let (extended, values) = header.index_to(Some(&north), message, coords).unwrap();
    assert_eq!(bits(&values), expected, "extended");
    exact(&extended);
    assert!(extended.reaches(coords));
    assert!(extended.indexed_rsis() >= north.indexed_rsis());

    let last = lattice.lat_first + (lattice.nj - 1) as f64 * lattice.lat_inc;
    let south: Vec<_> = (0..lattice.ni)
        .map(|i| (lattice.lon_first + i as f64 * lattice.lon_inc, last))
        .collect();
    assert_eq!(
        complete.needs(&south).unwrap().last_rsi,
        complete.indexed_rsis() - 1
    );
    let (whole, values) = header.index_to(Some(&extended), message, &south).unwrap();
    assert_eq!(bits(&values), bits(&decoded(message, &south)), "last row");
    assert!(whole.complete());
    assert_eq!(whole.rsi_offsets, complete.rsi_offsets);
}

/// `grib` rejects a message cut short, so a prefix is parsed through a stub
/// of its sections: the same header as the whole message's, from any cut
/// past Section 7's header.
#[test]
fn a_prefix_parses_as_the_whole_message() {
    let message = fixture();
    let len = message.len();
    let header = Header::parse(&message).unwrap();
    assert!(Header::parse(&message[..MIN_PREFIX]).is_err());
    for cut in [
        header.payload_start,
        header.payload_start + 1,
        MIN_PREFIX,
        len / 2,
        len,
    ] {
        assert_eq!(
            Header::parse_prefix(&message[..cut], len).unwrap(),
            Some(header),
            "cut {cut}"
        );
    }
    for cut in [0, 10, 16, 40, header.payload_start - 1] {
        assert_eq!(Header::parse_prefix(&message[..cut], len).unwrap(), None);
    }
    // The indexed length must be the message's, Section 7 its last.
    assert!(Header::parse_prefix(&message[..MIN_PREFIX], len - 1).is_err());
    let mut two = message.clone();
    two.extend(&message);
    let total = two.len() as u64;
    two[8..16].copy_from_slice(&total.to_be_bytes());
    assert!(Header::parse_prefix(&two[..MIN_PREFIX], two.len()).is_err());
    let mut other = message.clone();
    other[0] = b'X';
    assert!(Header::parse_prefix(&other[..MIN_PREFIX], len).is_err());
    let simple = crate::test_support::message(0, 280.0, [0, 2, 4, 6], 103, 2);
    assert!(matches!(
        Header::parse_prefix(&simple, simple.len()),
        Err(PointError::Unsupported("not CCSDS packing"))
    ));
}

/// Cold, short, extended and warm reads on the committed field: north,
/// south, the last RSI, a MULTIPOINT batch spanning most of the field, and
/// points off the grid. A read on from a partial index starts mid-byte at
/// least once, so libaec's offsets after a seek are proved exact.
#[test]
fn partial_reads_sample_like_the_full_decode() {
    let message = fixture();
    let (mut mid_byte, mut continued) = (false, false);
    for coords in [
        vec![(24.96, 60.32)],
        vec![(151.2, -33.9)],
        vec![(0.0, -89.9), (-180.0, -90.0)],
        vec![
            (24.96, 60.32),
            (139.8, 35.6),
            (151.2, -33.9),
            (-70.0, -60.0),
        ],
        vec![(0.0, 89.9)],
    ] {
        let reads = assert_partial_reads(&message, &coords);
        mid_byte |= reads.mid_byte;
        continued |= reads.continued;
    }
    assert!(mid_byte, "no read on from a partial index started mid-byte");
    assert!(continued, "no short prefix needed a continuation");
    assert_partial_reads(&message, &[(10.0, 91.0)]);
}

/// A prefix predicted from the message's own index as a same-run sibling
/// holds the needed RSIs with at most 2 % to spare; a linear prediction
/// stays within the message.
#[test]
fn prefix_predictions_cover_the_needed_bytes() {
    let message = fixture();
    let len = message.len();
    let (index, _) = Header::parse(&message)
        .unwrap()
        .index(&message, &[])
        .unwrap();
    for lat in [89.0, 60.0, 35.0, 0.0, -34.0, -89.9] {
        let coords = [(24.0, lat)];
        let needed = index.rsi_bytes(&index.needs(&coords).unwrap()).unwrap().end;
        let sibling = index.predict_prefix(&coords, len, Kin::SameRun);
        assert!(sibling >= needed.min(MIN_PREFIX).max(needed), "{lat}");
        assert!(
            sibling <= (needed as f64 * SIBLING_MARGIN).ceil() as usize + 1
                || sibling == MIN_PREFIX,
            "{lat}: {sibling} for {needed}"
        );
        let linear = index.predict_prefix(&coords, len, Kin::OtherParameter);
        assert!((MIN_PREFIX..=len).contains(&linear), "{lat}");
    }
    // Nothing to decode: just the header.
    assert_eq!(
        index.predict_prefix(&[(0.0, 95.0)], len, Kin::SameRun),
        MIN_PREFIX
    );
}

/// Bit identity and timings on any local GRIB2 files, one message each:
/// `MC_CCSDS_FILES=a.grib2:b.grib2 cargo test -p engine-grib --release
/// ccsds_files_are_bit_identical -- --ignored --nocapture`. Prints
/// measurements; no thresholds.
#[test]
#[ignore = "needs MC_CCSDS_FILES; prints measurements"]
fn ccsds_files_are_bit_identical() {
    let files = std::env::var("MC_CCSDS_FILES").expect("MC_CCSDS_FILES");
    for path in files.split(':').filter(|p| !p.is_empty()) {
        let message = std::fs::read(path).unwrap();
        let start = Instant::now();
        let grid = decode_message(&message, "field").unwrap();
        let full_ms = start.elapsed().as_secs_f64() * 1e3;
        let header = Header::parse(&message).unwrap();
        assert_full_decode_metadata(&message);
        let lattice = header.lattice;
        let mut coords = edge_points();
        let per_rsi = header.packing.aec.rsi_samples();
        let rsis = header.packing.values.div_ceil(per_rsi);
        for rsi in (1..rsis).step_by((rsis / 50).max(1)).chain([rsis - 1]) {
            coords.extend(straddling(&lattice, per_rsi, rsi));
        }
        let expected = bits(
            &coords
                .iter()
                .map(|&(lon, lat)| grid.bilinear_value(lon, lat))
                .collect::<Vec<_>>(),
        );
        let start = Instant::now();
        let (index, cold) = header.index(&message, &coords).unwrap();
        let index_ms = start.elapsed().as_secs_f64() * 1e3;
        assert_eq!(bits(&cold), expected, "{path}: while indexing");
        let start = Instant::now();
        let warm = index.sample(&message, &coords).unwrap();
        let warm_ms = start.elapsed().as_secs_f64() * 1e3;
        assert_eq!(bits(&warm), expected, "{path}: through the index");
        let start = Instant::now();
        let one = index.sample(&message, &[(24.96, 60.32)]).unwrap();
        let point_us = start.elapsed().as_secs_f64() * 1e6;
        assert_eq!(
            one[0].map(f64::to_bits),
            grid.bilinear_value(24.96, 60.32).map(f64::to_bits)
        );
        // The index decode through a reused chunk buffer against one
        // buffer of the whole field, best of three each.
        let payload = header.payload(&message).unwrap();
        let params = header.packing.aec;
        let decode_ms = |chunk: usize| {
            (0..3)
                .map(|_| {
                    let start = Instant::now();
                    aec::decode_indexed(&params, payload, header.packing.values, chunk, |_, _| {
                        Ok::<_, AecError>(())
                    })
                    .unwrap();
                    start.elapsed().as_secs_f64() * 1e3
                })
                .fold(f64::INFINITY, f64::min)
        };
        let (chunked_ms, whole_ms) = (decode_ms(CHUNK_SAMPLES), decode_ms(header.packing.values));
        let mut identical = 0;
        for rsi in 0..rsis {
            let start = rsi * per_rsi;
            let end = (start + per_rsi).min(grid.values.len());
            assert_eq!(
                f32_bits(&rsi_values(&index, &message, rsi)),
                f32_bits(&grid.values[start..end]),
                "{path}: RSI {rsi}"
            );
            identical += end - start;
        }
        eprintln!(
            "{path}: {}x{} lon_first={} bits={} flags={} {} RSIs index={} B; \
             {} points and all {identical} samples bit-identical; full decode {full_ms:.1} ms, \
             index+sample {index_ms:.1} ms, {} points via index {warm_ms:.2} ms, one point {point_us:.0} µs; \
             index decode {chunked_ms:.1} ms in {CHUNK_SAMPLES}-sample chunks, {whole_ms:.1} ms into one field buffer; \
             header metadata equals the full decode's",
            lattice.ni,
            lattice.nj,
            lattice.lon_first,
            header.packing.aec.bits_per_sample,
            header.packing.aec.flags,
            rsis,
            index.size_bytes(),
            coords.len(),
            coords.len(),
        );
        // #1301: cold, short, extended and warm partial reads, bit for bit.
        let mut shares = Vec::new();
        for (name, coords) in [
            ("60°N", vec![(24.96, 60.32)]),
            ("35°N", vec![(139.8, 35.6)]),
            ("34°S", vec![(151.2, -33.9)]),
            (
                "MULTIPOINT",
                vec![(24.96, 60.32), (151.2, -33.9), (0.0, -89.9)],
            ),
        ] {
            let reads = assert_partial_reads(&message, &coords);
            shares.push(format!(
                "{name} prefix {:.1} % span {} B",
                100.0 * reads.prefix as f64 / message.len() as f64,
                reads.span
            ));
        }
        assert_partial_reads(&message, &edge_points());
        eprintln!("  partial reads bit-identical: {}", shares.join(", "));
        // #1337: a cold index of the whole message for one point, complete
        // as before and through the point's RSIs, best of five each.
        let best = |f: &mut dyn FnMut()| {
            (0..5)
                .map(|_| {
                    let start = Instant::now();
                    f();
                    start.elapsed().as_secs_f64() * 1e3
                })
                .fold(f64::INFINITY, f64::min)
        };
        let mut cold = Vec::new();
        for (name, point) in [
            ("60°N", (24.96, 60.32)),
            ("35°N", (139.8, 35.6)),
            ("34°S", (151.2, -33.9)),
        ] {
            let complete_ms = best(&mut || {
                header.index(&message, &[point]).unwrap();
            });
            let through_ms = best(&mut || {
                header.index_to(None, &message, &[point]).unwrap();
            });
            cold.push(format!(
                "{name} {through_ms:.2} of {complete_ms:.2} ms ({:.0} %)",
                100.0 * through_ms / complete_ms
            ));
        }
        eprintln!(
            "  cold index through the point against complete: {}",
            cold.join(", ")
        );
        // Reads on predicted without a sibling, from every RSI start to 1,
        // 3, 10 and 60 RSIs ahead: how many fell short, and the most a
        // prediction overshot.
        let complete = index.rsi_offsets.len();
        let (mut short, mut reads, mut overshoot) = (0, 0, 0f64);
        for anchor in 1..complete - 1 {
            let partial = CcsdsIndex {
                header,
                rsi_offsets: index.rsi_offsets[..=anchor].into(),
            };
            for ahead in [0, 2, 9, 59] {
                let last = (anchor + ahead).min(complete - 1);
                let actual = index.end_bits(last).unwrap() - index.rsi_offsets[anchor] as usize;
                let end_sample = ((last + 1) * per_rsi).min(header.packing.values);
                let predicted = partial.read_on_bits(end_sample);
                reads += 1;
                if predicted < actual as f64 {
                    short += 1;
                }
                overshoot = overshoot.max(predicted / actual as f64);
            }
        }
        eprintln!(
            "  read on without a sibling: {short} of {reads} short, at most ×{overshoot:.2} the bits needed"
        );
    }
    // Messages of one parameter at other steps of the run, named `x.grib2`
    // and `x-9h.grib2`: header sizes, and the prefix one predicts for the
    // other against what the other needs.
    let paths: Vec<&str> = files.split(':').filter(|p| !p.is_empty()).collect();
    for path in &paths {
        let Some(other) = paths
            .iter()
            .find(|other| **other == path.replace(".grib2", "-9h.grib2"))
        else {
            continue;
        };
        let (a, b) = (std::fs::read(path).unwrap(), std::fs::read(other).unwrap());
        let (index_a, _) = Header::parse(&a).unwrap().index(&a, &[]).unwrap();
        let (index_b, _) = Header::parse(&b).unwrap().index(&b, &[]).unwrap();
        let (ha, hb) = (index_a.header, index_b.header);
        let mut errors = Vec::new();
        for lat in [75.0, 60.0, 45.0, 35.0, 10.0, -10.0, -34.0, -60.0] {
            let coords = [(24.0, lat)];
            let needed = index_b
                .rsi_bytes(&index_b.needs(&coords).unwrap())
                .unwrap()
                .end;
            for (index, kin) in [(&index_a, Kin::SameRun), (&index_a, Kin::OtherParameter)] {
                let predicted = index.predict_prefix(&coords, b.len(), kin);
                errors.push(format!(
                    "{lat}° {kin:?} {:+.2} %",
                    100.0 * (predicted as f64 / needed as f64 - 1.0)
                ));
                if kin == Kin::SameRun {
                    assert!(predicted >= needed, "{lat}: {predicted} < {needed}");
                }
            }
        }
        eprintln!(
            "{other} from {path}: header {} / {} B, overhead {} / {} B; prediction past need: {}",
            ha.payload_start,
            hb.payload_start,
            ha.message_len - ha.payload_len,
            hb.message_len - hb.payload_len,
            errors.join(", ")
        );
    }
}
