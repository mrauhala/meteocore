//! The 5.3 point reader against the `grib` crate's full decode, bit for bit
//! (#1302): the committed GFS fields (1°, one with a bitmap), synthetic
//! fields over every option the crate decodes, and malformed input.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use super::*;
use crate::reader::decode_message;
use crate::test_support::{Complex, Value};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../testdata/grib-gfs-complex/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn temperature() -> Vec<u8> {
    fixture("gfs-tmp2m-1deg.grib2")
}

fn soil() -> Vec<u8> {
    fixture("gfs-soilw-1deg.grib2")
}

fn bits(values: &[Option<f64>]) -> Vec<Option<u64>> {
    values.iter().map(|v| v.map(f64::to_bits)).collect()
}

fn f32_bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|v| v.to_bits()).collect()
}

/// `coords` sampled from the full decode of `message`.
fn decoded(message: &[u8], coords: &[(f64, f64)]) -> Vec<Option<f64>> {
    let grid = decode_message(message, "x").unwrap();
    coords
        .iter()
        .map(|&(lon, lat)| grid.bilinear_value(lon, lat))
        .collect()
}

/// Every node's value through this reader, decoded from the field's start.
fn every_value(message: &[u8]) -> Vec<f32> {
    let header = Header::parse(message).unwrap();
    let start = header.start_index(message).unwrap();
    let nodes: Vec<usize> = (0..header.nodes()).collect();
    start.decode_nodes(message, 0, &nodes, true).unwrap().1
}

/// A copy of `index` keeping only its first `rows` checkpoints, as a decode
/// of a prefix leaves it.
fn first_rows(index: &ComplexIndex, rows: usize) -> ComplexIndex {
    ComplexIndex {
        header: index.header,
        extra: index.extra,
        descriptors: index.descriptors.clone(),
        rows: index.rows[..rows].into(),
    }
}

/// Seams, poles, the grid's own first and last nodes and points just off
/// them, on a grid sweep.
fn edge_points() -> Vec<(f64, f64)> {
    let mut coords = Vec::new();
    for lat in [90.0, 89.9, 60.32, 35.6, 0.0, -0.1, -33.9, -89.9, -90.0] {
        for lon in [
            -180.0, -0.1, 0.0, 0.1, 24.96, 151.2, 179.9, 359.0, 359.5, 360.0,
        ] {
            coords.push((lon, lat));
        }
    }
    coords.extend([
        (10.0, 90.1),
        (10.0, -90.1),
        (f64::NAN, 0.0),
        (0.0, f64::INFINITY),
    ]);
    for i in 0..61 {
        coords.push((-180.0 + i as f64 * 5.93, 89.5 - i as f64 * 2.97));
    }
    coords
}

/// `coords` read as the engine reads them from parts of `message`, each bit
/// for bit `expected`:
/// - cold: an index of no rows from a prefix through the descriptors, then
///   the prefix through [`ComplexIndex::span`], and one byte less fails;
/// - warm: the span of the complete index alone, or for a bitmapped field
///   the prefix [`ComplexIndex::prefix_bound`] names;
/// - read on: the span of indexes of only the first rows, whose new
///   checkpoints are the complete index's;
/// - whole (#1337): the whole message indexed only through the rows read,
///   and an index of the first row's points extended to them and on to the
///   last rows, the complete index, from the same bytes.
fn assert_partial_reads(
    message: &[u8],
    index: &ComplexIndex,
    coords: &[(f64, f64)],
    expected: &[Option<u64>],
) {
    let header = *index.header();
    let bitmap = index.bitmap_in(message, 0).unwrap();
    let empty = header.start_index(&message[..header.data_start()]).unwrap();
    assert!(header
        .start_index(&message[..header.data_start() - 1])
        .is_none());
    let span = empty.span(coords, bitmap).unwrap();
    let prefix = &message[..span.end.max(header.data_start())];
    let (values, extended) = empty.read(prefix, 0, coords).unwrap();
    assert_eq!(bits(&values), expected, "cold {coords:?}");
    if let Some(extended) = extended {
        assert_eq!(extended.rows[..], index.rows[..extended.rows.len()]);
    }
    if span.end > header.data_start() {
        let plan = Plan::new(&header.lattice, coords);
        let last = packed_indices(&plan.nodes, bitmap)
            .into_iter()
            .flatten()
            .max()
            .unwrap();
        let (last_bit, width) = empty.locate(&Checkpoint::default(), last).unwrap();
        if width > 0 {
            assert_eq!(
                header.data_start() + (last_bit + u64::from(width)).div_ceil(8) as usize,
                span.end
            );
            assert!(matches!(
                empty.read(&prefix[..prefix.len() - 1], 0, coords),
                Err(PointError::Invalid(_))
            ));
        }
    }

    let warm = |index: &ComplexIndex| -> (Vec<Option<f64>>, Option<ComplexIndex>) {
        if index.bitmapped() {
            let bound = index.prefix_bound(coords).unwrap_or(message.len());
            let prefix = &message[..bound];
            let span = index
                .span(coords, index.bitmap_in(prefix, 0).unwrap())
                .unwrap();
            assert!(span.end <= bound, "{span:?} past {bound}");
            index.read(prefix, 0, coords).unwrap()
        } else {
            let span = index.span(coords, None).unwrap();
            index
                .read(&message[span.clone()], span.start, coords)
                .unwrap()
        }
    };
    let (values, extended) = warm(index);
    assert_eq!(bits(&values), expected, "warm {coords:?}");
    assert!(extended.is_none());
    let nj = header.lattice.nj;
    for rows in [1, 2, nj / 3, nj - 1] {
        let partial = first_rows(index, rows);
        let (values, extended) = warm(&partial);
        assert_eq!(bits(&values), expected, "read on from {rows} rows");
        if let Some(extended) = extended {
            assert!(extended.rows.len() > rows);
            assert_eq!(extended.rows[..], index.rows[..extended.rows.len()]);
        }
    }
    assert_whole_reads(message, index, coords, expected);
}

/// The whole message indexed only through the rows `coords` read (#1337),
/// bit for bit `expected`, with exactly the checkpoints `index`, the
/// complete index, has: from nothing, stopping at the row after the last
/// one read when no bitmap masks the rows after it; on from an index of the
/// first row's points; then on to the last value and the last row, which
/// completes the index.
fn assert_whole_reads(
    message: &[u8],
    index: &ComplexIndex,
    coords: &[(f64, f64)],
    expected: &[Option<u64>],
) {
    let header = *index.header();
    let lattice = header.lattice;
    let exact = |partial: &ComplexIndex| {
        assert_eq!(partial.rows[..], index.rows[..partial.rows.len()], "rows");
    };
    // A decode passes rows only to read a value: points whose every node
    // the bitmap masks read none, and index no row.
    let plan = Plan::new(&lattice, coords);
    let bitmap = index.bitmap_in(message, 0).unwrap();
    let read = packed_indices(&plan.nodes, bitmap)
        .iter()
        .any(Option::is_some);
    let (through, values) = header.index_to(message, coords).unwrap();
    assert_eq!(
        bits(&values),
        expected,
        "whole, through the rows {coords:?}"
    );
    exact(&through);
    assert!(through.reaches(coords) || !read, "{coords:?}");
    if let (Some(&last), false) = (plan.nodes.last(), index.bitmapped()) {
        assert!(through.indexed_rows() <= last / lattice.ni + 2);
    }

    let first = [(lattice.lon_first, lattice.lat_first)];
    let (north, _) = header.index_to(message, &first).unwrap();
    let (values, extended) = north.read_message(message, coords).unwrap();
    assert_eq!(bits(&values), expected, "extended {coords:?}");
    let extended = extended.unwrap_or(north);
    exact(&extended);
    assert!(extended.reaches(coords) || !read, "{coords:?}");

    // The last value's node and the field's last node, which a bitmap may
    // mask, with the rows after the last value.
    let nodes = lattice.ni * lattice.nj;
    let last_value = (0..nodes)
        .rev()
        .find(|&n| bitmap.is_none_or(|bitmap| is_set(bitmap, n)))
        .unwrap();
    let node = |n: usize| {
        (
            lattice.lon_first + (n % lattice.ni) as f64 * lattice.lon_inc,
            lattice.lat_first + (n / lattice.ni) as f64 * lattice.lat_inc,
        )
    };
    let south = [node(last_value), node(nodes - 1)];
    let (values, whole) = extended.read_message(message, &south).unwrap();
    assert_eq!(bits(&values), bits(&decoded(message, &south)), "last rows");
    let whole = whole.unwrap_or(extended);
    assert!(whole.complete());
    assert_eq!(whole.rows[..], index.rows[..]);
}

/// Every value bit for bit the full decode's; `coords` sampled while
/// indexing, through the index from the whole message as one batch and one
/// point at a time, and from parts of the message
/// ([`assert_partial_reads`]). Returns the complete index.
fn assert_identical(message: &[u8], coords: &[(f64, f64)]) -> ComplexIndex {
    let grid = decode_message(message, "x").unwrap();
    assert_eq!(f32_bits(&every_value(message)), f32_bits(&grid.values));
    let expected = bits(&decoded(message, coords));
    let header = Header::parse(message).unwrap();
    let (index, cold) = header.index(message, coords).unwrap();
    assert_eq!(index.indexed_rows(), header.lattice.nj);
    assert_eq!(bits(&cold), expected, "while indexing");
    assert_eq!(
        bits(&index.sample(message, coords).unwrap()),
        expected,
        "batch"
    );
    assert_partial_reads(message, &index, coords, &expected);
    for (&coord, &expected) in coords.iter().zip(&expected) {
        let single = index.sample(message, &[coord]).unwrap();
        assert_eq!(single[0].map(f64::to_bits), expected, "{coord:?}");
        assert_partial_reads(message, &index, &[coord], &[expected]);
    }
    index
}

/// What the full decode of `message` reports to the metadata caches, which
/// the header must report alike.
fn decode_metadata(message: &[u8]) -> MessageMetadata {
    let grid = decode_message(message, "x").unwrap();
    let metadata = Header::parse(message).unwrap().metadata;
    assert_eq!(metadata.triple, grid.triple);
    assert_eq!(metadata.centre, grid.centre);
    assert_eq!(metadata.first_surface_type, grid.first_surface_type);
    assert_eq!(metadata.first_surface_value, grid.first_surface_value);
    assert_eq!(metadata.statistic, grid.statistic);
    assert_eq!(metadata.uv_frame, grid.uv_frame);
    assert_eq!(
        metadata.geometry,
        GridGeometry::new(
            grid.ni,
            grid.nj,
            grid.lon_first,
            grid.lat_first,
            grid.lon_inc,
            grid.lat_inc
        )
    );
    metadata
}

#[test]
fn the_fixtures_are_gfs_complex_packing() {
    for (message, bitmapped, values, groups) in [
        (temperature(), false, 65_160, 2309),
        (soil(), true, 22_071, 508),
    ] {
        let header = Header::parse(&message).unwrap();
        assert_eq!(
            header.lattice,
            Lattice {
                ni: 360,
                nj: 181,
                lon_first: 0.0,
                lat_first: 90.0,
                lon_inc: 1.0,
                lat_inc: -1.0,
            }
        );
        assert_eq!(header.bitmap.is_some(), bitmapped);
        assert_eq!(header.values, values);
        assert_eq!(header.layout.groups, groups);
        assert_eq!((header.layout.order, header.layout.octets), (2, 2));
        assert_eq!(header.layout.missing, 0);
        decode_metadata(&message);
        let (index, _) = header.index(&message, &[]).unwrap();
        assert_eq!(index.indexed_rows(), 181);
        // 181 checkpoints and the descriptors, 24 bits per group.
        assert!(
            index.size_bytes() < 181 * size_of::<Checkpoint>() + groups * 3 + 512,
            "{}",
            index.size_bytes()
        );
    }
}

#[test]
fn fixture_points_are_bit_identical_to_the_full_decode() {
    for message in [temperature(), soil()] {
        assert_identical(&message, &edge_points());
    }
}

/// A smooth field with some noise: `nodes` packed integers around 20 000.
fn smooth(nodes: usize, seed: u64) -> Vec<i64> {
    let mut state = seed;
    (0..nodes)
        .map(|k| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let wave = (k as f64 / 37.0).sin() * 900.0 + (k as f64 / 1300.0).cos() * 4000.0;
            20_000 + wave as i64 + (state % 23) as i64
        })
        .collect()
}

/// Every option `grib` decodes, on small and global grids.
#[test]
fn synthetic_layouts_are_bit_identical() {
    let global = Complex::global();
    let nodes = (global.ni * global.nj) as usize;
    let present =
        |values: &[i64]| -> Vec<Value> { values.iter().map(|&v| Value::Present(v)).collect() };
    let field = smooth(nodes, 0x9e37_79b9_7f4a_7c15);
    let mut cases: Vec<(String, Complex, Vec<Value>)> = vec![
        ("second order".into(), global.clone(), present(&field)),
        (
            "first order".into(),
            Complex {
                order: 1,
                ..global.clone()
            },
            present(&field),
        ),
    ];
    // Descriptor octets, a negative first value, wide differences.
    for (octets, scale) in [(1u8, 1i64), (3, 300), (4, 70_000)] {
        let values: Vec<i64> = field
            .iter()
            .map(|v| {
                let v = (v - 20_000) * scale - 5;
                if octets == 1 {
                    v.clamp(-60, 60)
                } else {
                    v
                }
            })
            .collect();
        cases.push((
            format!("{octets} octets"),
            Complex {
                octets,
                binary: -3,
                decimal: 1,
                reference: -40.5,
                ..global.clone()
            },
            present(&values),
        ));
    }
    // Missing values: one kind, then both, at the start, in runs that fill
    // whole groups, and alone.
    let missing = |kinds: u8| -> Vec<Value> {
        field
            .iter()
            .enumerate()
            .map(|(k, &v)| {
                let (run, every) = (k % 997, k % 13);
                if kinds == 1 && (run <= 60 || every == 0) || run <= 30 || every == 0 {
                    Value::Missing
                } else if kinds == 2 && (run <= 60 || every == 5) {
                    Value::SecondaryMissing
                } else {
                    Value::Present(v)
                }
            })
            .collect()
    };
    for kinds in [1, 2] {
        for order in [1, 2] {
            cases.push((
                format!("missing values {kinds}, order {order}"),
                Complex {
                    missing: kinds,
                    order,
                    ..global.clone()
                },
                missing(kinds),
            ));
        }
    }
    // A bitmap masking the first and last rows and patches between, with
    // and without missing values among the rest.
    let bitmap: Vec<bool> = (0..nodes)
        .map(|n| {
            let (row, col) = (n / 360, n % 360);
            row > 0 && row < 178 && (col / 7 + row / 5) % 3 != 0
        })
        .collect();
    let encoded = bitmap.iter().filter(|&&set| set).count();
    cases.push((
        "bitmap".into(),
        Complex {
            bitmap: Some(bitmap.clone()),
            ..global.clone()
        },
        present(&field[..encoded]),
    ));
    cases.push((
        "bitmap and missing values".into(),
        Complex {
            bitmap: Some(bitmap),
            missing: 2,
            ..global.clone()
        },
        missing(2)[..encoded].to_vec(),
    ));
    // Group lengths in steps of two, a constant field, and other scales.
    cases.push((
        "length increment".into(),
        Complex {
            lengths: vec![4, 8, 6, 30, 12],
            length_increment: 2,
            ..global.clone()
        },
        present(&field),
    ));
    cases.push((
        "constant".into(),
        Complex {
            reference: 101_325.0,
            binary: 2,
            decimal: -1,
            ..global.clone()
        },
        present(&vec![7; nodes]),
    ));
    // A regional grid in one group, and one in groups of one.
    let regional = Complex {
        ni: 7,
        nj: 5,
        lon_first: 10_000_000,
        lat_first: 60_000_000,
        step: 500_000,
        lengths: vec![100],
        ..global.clone()
    };
    cases.push(("one group".into(), regional.clone(), present(&field[..35])));
    cases.push((
        "groups of one".into(),
        Complex {
            lengths: vec![1],
            ..regional
        },
        present(&field[..35]),
    ));

    let mut coords = edge_points();
    coords.extend([(10.2, 59.3), (12.9, 58.1), (13.0, 58.0), (11.0, 60.0)]);
    for (name, spec, values) in cases {
        let message = spec.message(&values);
        let header = Header::parse(&message).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(header.layout.order, spec.order, "{name}");
        assert_eq!(header.layout.missing, spec.missing, "{name}");
        let checked = catch_unwind(AssertUnwindSafe(|| assert_identical(&message, &coords)));
        assert!(checked.is_ok(), "{name}");
    }
}

/// Section offsets of a synthetic message without a bitmap: Section 3 at
/// 37, Section 5 (template 5.3's 49 octets) at 143, Section 6 at 192.
const S3: usize = 37;
const S5: usize = 143;
const S6: usize = 192;

#[test]
fn other_layouts_are_unsupported() {
    let values: Vec<_> = smooth(360 * 181, 7)
        .into_iter()
        .map(Value::Present)
        .collect();
    let message = Complex::global().message(&values);
    assert!(Header::parse(&message).is_ok());
    let reason = |patch: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = message.clone();
        patch(&mut bytes);
        match Header::parse(&bytes) {
            Err(PointError::Unsupported(reason)) => reason,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(reason(&|b| b[S5 + 20] = 1), "original field type");
    assert_eq!(reason(&|b| b[S5 + 21] = 2), "group splitting method");
    assert_eq!(reason(&|b| b[S5 + 22] = 3), "missing value management");
    assert_eq!(reason(&|b| b[S5 + 47] = 3), "order of spatial differencing");
    assert_eq!(reason(&|b| b[S5 + 31..S5 + 35].fill(0)), "no groups");
    assert_eq!(reason(&|b| b[S5 + 19] = 32), "descriptor width");
    assert_eq!(reason(&|b| b[S5 + 36] = 40), "descriptor width");
    assert_eq!(
        reason(&|b| b[S5 + 15..S5 + 17].copy_from_slice(&127u16.to_be_bytes())),
        "binary scale factor"
    );
    assert_eq!(reason(&|b| b[S6 + 5] = 254), "bitmap indicator");
    assert_eq!(reason(&|b| b[S3 + 71] = 0x40), "scan mode");
    for octets in [0, 5] {
        let mut bytes = message.clone();
        bytes[S5 + 48] = octets;
        assert!(matches!(Header::parse(&bytes), Err(PointError::Invalid(_))));
    }
    let simple = crate::test_support::message(0, 280.0, [0, 2, 4, 6], 103, 2);
    assert!(matches!(
        Header::parse(&simple),
        Err(PointError::Unsupported(
            "not complex packing with spatial differencing"
        ))
    ));
    assert_eq!(crate::ccsds::data_template(&message), Some(3));
    assert_eq!(crate::ccsds::data_template(&simple), Some(0));
    assert_eq!(crate::ccsds::data_template(&message[..S5 + 10]), None);
}

/// `grib` rejects a message cut short, so a prefix parses through a stub of
/// its sections: the same header as the whole message's from any cut past
/// Section 7's header, the bitmap included.
#[test]
fn a_prefix_parses_as_the_whole_message() {
    for message in [temperature(), soil()] {
        let len = message.len();
        let header = Header::parse(&message).unwrap();
        for cut in [
            header.payload_start,
            header.payload_start + 1,
            header.data_start(),
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
        assert!(Header::parse_prefix(&message[..len / 2], len - 1).is_err());
    }
}

/// A message cut short, Section 7 declared shorter, corrupt descriptors and
/// data, and parts that do not hold what a read needs: errors or exactly
/// the full decode's values, never a panic. `grib` panics on some of these
/// (slices past Section 7, or integer overflow in a debug build); this
/// reader must not.
#[test]
fn malformed_messages_fail_or_agree_with_the_full_decode() {
    let coords = edge_points();
    for message in [temperature(), soil()] {
        let header = Header::parse(&message).unwrap();
        let len = message.len();
        let expected = bits(&decoded(&message, &coords));

        // Cut short anywhere: never a wrong value, never a panic.
        for cut in (1..len).step_by(len / 97).chain([len - 5, len - 1]) {
            let short = &message[..cut];
            assert!(Header::parse(short).is_err(), "cut {cut}");
            let Ok(Some(prefix)) = Header::parse_prefix(short, len) else {
                continue;
            };
            assert_eq!(prefix, header);
            let Some(empty) = prefix.start_index(short) else {
                assert!(cut < header.data_start());
                continue;
            };
            let bitmap = empty.bitmap_in(short, 0).unwrap();
            let span = empty.span(&coords, bitmap).unwrap();
            match empty.read(short, 0, &coords) {
                Ok((values, _)) => {
                    assert!(span.end <= cut, "cut {cut}");
                    assert_eq!(bits(&values), expected);
                }
                Err(PointError::Invalid(_)) => assert!(span.end > cut, "cut {cut}"),
                Err(other) => panic!("cut {cut}: {other:?}"),
            }
        }

        // Section 7 declaring fewer bytes.
        for less in [1, 100, len / 2] {
            let mut cut = message[..len - 4 - less].to_vec();
            cut.extend(b"7777");
            let s7 = header.payload_start - 5;
            let s7_len = u32::from_be_bytes(cut[s7..s7 + 4].try_into().unwrap()) - less as u32;
            cut[s7..s7 + 4].copy_from_slice(&s7_len.to_be_bytes());
            let total = cut.len() as u64;
            cut[8..16].copy_from_slice(&total.to_be_bytes());
            let ours = catch_unwind(|| Header::parse(&cut).and_then(|h| h.index(&cut, &coords)));
            assert!(
                matches!(ours, Ok(Err(PointError::Invalid(_)))),
                "less {less}"
            );
            // Through the points only (#1337), the decode may stop before
            // the missing data: never a panic, never another error.
            for coords in [&coords[..], &[(24.96, 60.32)]] {
                let through =
                    catch_unwind(|| Header::parse(&cut).and_then(|h| h.index_to(&cut, coords)));
                assert!(
                    matches!(through, Ok(Ok(_) | Err(PointError::Invalid(_)))),
                    "less {less}, through {} points",
                    coords.len()
                );
            }
        }

        // Corrupt extra descriptors, descriptors and data.
        let start = header.payload_start;
        let layout = header.layout;
        for (at, size) in [
            (0, 2 * layout.octets),
            (layout.refs, 64),
            (layout.widths, 32),
            (layout.lengths + 8, 16),
            (layout.data + 100, 64),
            (header.payload_len - 64, 64),
        ] {
            for byte in [0x00, 0xff, 0x5a] {
                let mut corrupt = message.clone();
                corrupt[start + at..start + at + size].fill(byte);
                let full = catch_unwind(|| decode_message(&corrupt, "x"));
                let ours = catch_unwind(|| {
                    Header::parse(&corrupt).and_then(|h| h.index(&corrupt, &coords))
                });
                let Ok(ours) = ours else {
                    panic!("panicked on corrupt bytes at {at}")
                };
                let Ok(through) = catch_unwind(|| {
                    Header::parse(&corrupt).and_then(|h| h.index_to(&corrupt, &coords))
                }) else {
                    panic!("panicked on corrupt bytes at {at}, through the points")
                };
                if let (Ok(Ok(grid)), Ok((index, values))) = (full, ours) {
                    let expected: Vec<_> = coords
                        .iter()
                        .map(|&(lon, lat)| grid.bilinear_value(lon, lat))
                        .collect();
                    assert_eq!(bits(&values), bits(&expected), "corrupt at {at}");
                    assert_eq!(
                        bits(&index.sample(&corrupt, &coords).unwrap()),
                        bits(&expected)
                    );
                    let (_, values) = through.unwrap();
                    assert_eq!(bits(&values), bits(&expected), "through, corrupt at {at}");
                }
            }
        }

        // Parts that do not hold what a read needs.
        let (index, _) = header.index(&message, &[]).unwrap();
        let north = [(24.96, 60.32)];
        let mut longer = message.clone();
        longer.push(0);
        assert!(matches!(
            index.sample(&longer, &north),
            Err(PointError::Invalid(_))
        ));
        if index.bitmapped() {
            let past_bitmap = header.data_start();
            assert!(matches!(
                index.read(&message[past_bitmap..], past_bitmap, &north),
                Err(PointError::Invalid(_))
            ));
            assert!(index.span(&north, None).is_err());
        } else {
            let span = index.span(&north, None).unwrap();
            let late = span.start + 1;
            assert!(matches!(
                index.read(&message[late..span.end], late, &north),
                Err(PointError::Invalid(_))
            ));
        }
    }
}

/// A read's span starts at its first row's checkpoint and ends with its
/// last value: a point reads about two rows of data.
#[test]
fn a_warm_point_reads_two_rows_of_data() {
    let message = temperature();
    let (index, _) = Header::parse(&message)
        .unwrap()
        .index(&message, &[])
        .unwrap();
    let data = message.len() - index.header.data_start();
    for point in [
        (24.96, 60.32),
        (139.8, 35.6),
        (151.2, -33.9),
        (359.5, -89.5),
    ] {
        let span = index.span(&[point], None).unwrap();
        assert!(span.len() < 3 * data / 181, "{point:?}: {span:?} of {data}");
        let first_row = (90.0 - point.1).floor() as usize;
        let checkpoint = index.rows[first_row];
        let (bit, _) = index
            .locate(&checkpoint, checkpoint.packed as usize)
            .unwrap();
        assert_eq!(span.start, index.header.data_start() + (bit / 8) as usize);
    }
    // Nothing on the grid reads nothing.
    let off = [(0.0, 91.0)];
    let span = index.span(&off, None).unwrap();
    assert!(span.is_empty());
    let (values, extended) = index.read(&[], span.start, &off).unwrap();
    assert_eq!(values, vec![None]);
    assert!(extended.is_none());
}

/// An index predicts a prefix of a sibling message: of itself as a sibling
/// in the same run, at least what a point needs and at most 2 % more than
/// the end of its last row; linearly, a prefix within the message.
#[test]
fn prefix_predictions_cover_the_needed_bytes() {
    for message in [temperature(), soil()] {
        let len = message.len();
        let (index, _) = Header::parse(&message)
            .unwrap()
            .index(&message, &[])
            .unwrap();
        let bitmap = index.bitmap_in(&message, 0).unwrap();
        let row_start = |row: usize| {
            let checkpoint = index.rows[row];
            let (bit, _) = index
                .locate(&checkpoint, checkpoint.packed as usize)
                .unwrap();
            index.header.data_start() + (bit / 8) as usize
        };
        for lat in [85.0, 60.32, 35.6, 0.0, -33.9, -80.0] {
            let coords = [(24.0, lat)];
            let needed = index.span(&coords, bitmap).unwrap().end;
            let row = (90.0 - lat).floor() as usize + 1;
            let bound = index.prefix_bound(&coords).unwrap();
            assert!(needed <= bound && bound <= row_start(row + 1) + 1, "{lat}");
            let same = index.predict_prefix(&coords, len, Kin::SameRun);
            assert!(same >= needed, "{lat}: {same} < {needed}");
            assert!(
                same as f64 <= 1.02 * bound as f64 + 1.0,
                "{lat}: {same} for {needed}, bound {bound}"
            );
            let linear = index.predict_prefix(&coords, len, Kin::OtherParameter);
            assert!((MIN_PREFIX.min(len)..=len).contains(&linear));
            let end = index.header.predict_end(&coords);
            assert!(end > index.header.data_start() && end <= len);
        }
    }
}

/// Bit identity, timings and bytes per read on any local GRIB2 files in
/// template 5.3, one message each:
/// `MC_COMPLEX_FILES=a.grib2:b.grib2 cargo test -p engine-grib --release
/// complex_files_are_bit_identical -- --ignored --nocapture`. Files named
/// `x.grib2` and `x-f009.grib2` are also read as same-run siblings. Prints
/// measurements; no thresholds.
#[test]
#[ignore = "needs MC_COMPLEX_FILES; prints measurements"]
fn complex_files_are_bit_identical() {
    let files = std::env::var("MC_COMPLEX_FILES").expect("MC_COMPLEX_FILES");
    let points = [
        ("60°N", (24.96, 60.32)),
        ("35°N", (139.8, 35.6)),
        ("34°S", (151.2, -33.9)),
    ];
    let best = |f: &mut dyn FnMut()| {
        (0..3)
            .map(|_| {
                let start = Instant::now();
                f();
                start.elapsed().as_secs_f64() * 1e3
            })
            .fold(f64::INFINITY, f64::min)
    };
    let paths: Vec<&str> = files.split(':').filter(|p| !p.is_empty()).collect();
    for path in &paths {
        let message = std::fs::read(path).unwrap();
        let len = message.len();
        let grid = decode_message(&message, "x").unwrap();
        let full_ms = best(&mut || {
            decode_message(&message, "x").unwrap();
        });
        assert_eq!(
            f32_bits(&every_value(&message)),
            f32_bits(&grid.values),
            "{path}: every value"
        );
        decode_metadata(&message);
        let header = Header::parse(&message).unwrap();
        let coords = edge_points();
        let expected = bits(&decoded(&message, &coords));
        let (index, cold) = header.index(&message, &coords).unwrap();
        assert_eq!(bits(&cold), expected, "{path}");
        let index_ms = best(&mut || {
            header.index(&message, &coords).unwrap();
        });
        let point_ms = best(&mut || {
            index.sample(&message, &[(24.96, 60.32)]).unwrap();
        });
        assert_partial_reads(&message, &index, &coords, &expected);
        let bitmap = index.bitmap_in(&message, 0).unwrap();
        let mut reads = Vec::new();
        for (name, point) in points {
            let coords = [point];
            let expected = bits(&decoded(&message, &coords));
            assert_partial_reads(&message, &index, &coords, &expected);
            let cold = index.span(&coords, bitmap).unwrap().end;
            let warm = if index.bitmapped() {
                index.prefix_bound(&coords).unwrap()
            } else {
                index.span(&coords, None).unwrap().len()
            };
            // #1337: a cold index of the whole message, complete as before
            // and through the point's rows.
            let complete_ms = best(&mut || {
                header.index(&message, &coords).unwrap();
            });
            let through_ms = best(&mut || {
                header.index_to(&message, &coords).unwrap();
            });
            reads.push(format!(
                "{name} cold {cold} B ({:.1} %), warm {warm} B, cold index through the point \
                 {through_ms:.2} of {complete_ms:.2} ms ({:.0} %)",
                100.0 * cold as f64 / len as f64,
                100.0 * through_ms / complete_ms
            ));
        }
        eprintln!(
            "{path}: {len} B, {}x{}, {} values, {} groups, order {}, bitmap {}, data from {} B; \
             every value bit-identical; index {} B ({} rows); full decode {full_ms:.1} ms, \
             index {index_ms:.1} ms, one point from the index {:.0} µs\n  {}",
            header.lattice.ni,
            header.lattice.nj,
            header.values,
            header.layout.groups,
            header.layout.order,
            index.bitmapped(),
            header.data_start(),
            index.size_bytes(),
            index.indexed_rows(),
            point_ms * 1e3,
            reads.join("; ")
        );
    }
    for path in &paths {
        let Some(other) = paths
            .iter()
            .find(|other| **other == path.replace(".grib2", "-f009.grib2"))
        else {
            continue;
        };
        let (a, b) = (std::fs::read(path).unwrap(), std::fs::read(other).unwrap());
        let (index_a, _) = Header::parse(&a).unwrap().index(&a, &[]).unwrap();
        let (index_b, _) = Header::parse(&b).unwrap().index(&b, &[]).unwrap();
        let mut errors = Vec::new();
        for lat in [75.0, 60.32, 45.0, 35.6, 10.0, -10.0, -33.9, -60.0] {
            let coords = [(24.0, lat)];
            let needed = index_b.span(&coords, None).unwrap().end;
            for kin in [Kin::SameRun, Kin::OtherParameter] {
                let predicted = index_a.predict_prefix(&coords, b.len(), kin);
                errors.push(format!(
                    "{lat}° {kin:?} {:+.2} %",
                    100.0 * (predicted as f64 / needed as f64 - 1.0)
                ));
            }
        }
        eprintln!(
            "{other} from {path}: prediction past need: {}",
            errors.join(", ")
        );
    }
}
