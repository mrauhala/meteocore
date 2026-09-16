//! Reader for the data section of BUFR files

use std::io::Read;

use bitstream_io::{BigEndian, BitRead, BitReader};

use crate::{
    Error, ResolvedDescriptor, Value, XY, resolve_descriptors,
    sections::{DataDescriptionSection, DataSectionHeader},
    tables::{TableBEntry, TableDEntry, Tables},
};

/// A reader for parsing BUFR data sections.
pub struct DataReader<'a, R: Read> {
    data_spec: &'a DataSpec<'a>,
    current_subset_index: u16,
    reader: BitReader<std::io::Take<R>, BigEndian>,
    /// Stack for parsing nested data
    stack: smallvec::SmallVec<[StackEntry<'a>; 8]>,
    /// Current offset set by the "Change data width" operator
    width_offset: i8,
    /// Current offset set by the "Change scale" operator
    scale_offset: i8,
    character_width: Option<u32>,
}

/// Data specification for reading BUFR data section.
#[derive(Debug)]
pub struct DataSpec<'a> {
    /// The number of subsets in the data section
    pub number_of_subsets: u16,
    /// Indicates if the data is stored in "compressed" format (column oriented) or not
    pub is_compressed: bool,
    /// The sequence of descriptors stored in the header
    pub root_descriptors: Vec<ResolvedDescriptor<'a>>,
}

impl<'a> DataSpec<'a> {
    pub fn from_data_description(
        dds: &'a DataDescriptionSection,
        tables: &'a Tables,
    ) -> Result<Self, Error> {
        Ok(Self {
            number_of_subsets: dds.number_of_subsets,
            is_compressed: dds.flags.is_compressed,
            root_descriptors: resolve_descriptors(tables, &dds.descriptors)?,
        })
    }
}

impl<'a, R: Read> DataReader<'a, R> {
    pub fn new(mut reader: R, spec: &'a DataSpec<'a>) -> Result<DataReader<'a, R>, Error> {
        let data_section_header = DataSectionHeader::read(&mut reader)?;
        let data_length = data_section_header
            .section_length
            .checked_sub(4)
            .ok_or_else(|| Error::Invalid("Data section is shorter than its header".into()))?;
        Ok(DataReader {
            data_spec: spec,
            current_subset_index: 0,
            reader: BitReader::endian(reader.take(u64::from(data_length)), BigEndian),
            stack: smallvec::SmallVec::new(),
            scale_offset: 0,
            width_offset: 0,
            character_width: None,
        })
    }

    /// Unwraps this `DataReader`, returning the underlying reader.
    pub fn into_inner(self) -> R {
        self.reader.into_reader().into_inner()
    }
}

struct StackEntry<'a> {
    ty: StackEntryType,
    descriptors: &'a [ResolvedDescriptor<'a>],
    next: u16,
}

enum StackEntryType {
    Sequence,
    Replication { remaining: u16, in_item: bool },
}

impl<'a> StackEntry<'a> {
    fn new_sequence(descriptors: &'a [ResolvedDescriptor<'a>]) -> Self {
        Self {
            ty: StackEntryType::Sequence,
            descriptors,
            next: 0,
        }
    }

    fn new_replication(descriptors: &'a [ResolvedDescriptor<'a>], count: u16) -> Self {
        Self {
            ty: StackEntryType::Replication {
                remaining: count,
                in_item: false,
            },
            descriptors,
            next: descriptors.len() as u16,
        }
    }
}

pub(crate) fn three_bytes_to_u32(bytes: [u8; 3]) -> u32 {
    (bytes[0] as u32) << 16 | (bytes[1] as u32) << 8 | (bytes[2] as u32)
}

fn all_ones(bits: u32) -> u64 {
    if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

/// Event emitted by [`DataReader`].
#[derive(Debug)]
pub enum DataEvent {
    SubsetStart(u16),
    SubsetEnd,
    CompressedStart,
    ReplicationStart {
        idx: u16,
        count: u16,
    },
    ReplicationItemStart,
    ReplicationItemEnd,
    ReplicationEnd,
    SequenceStart {
        idx: u16,
        xy: XY,
    },
    SequenceEnd,
    OperatorHandled {
        idx: u16,
        x: u8,
        value: i32,
    },
    Data {
        idx: u16,
        xy: XY,
        value: Value,
    },
    CompressedData {
        idx: u16,
        xy: XY,
        values: Vec<Value>,
    },
    Eof,
}

impl<'a, R: Read> DataReader<'a, R> {
    /// Reads the next data event.
    pub fn read_event(&mut self) -> Result<DataEvent, Error> {
        if self.stack.is_empty() {
            if self.data_spec.is_compressed {
                if self.current_subset_index > 0 {
                    return Ok(DataEvent::Eof);
                }
            } else if self.current_subset_index == self.data_spec.number_of_subsets {
                return Ok(DataEvent::Eof);
            }

            self.width_offset = 0;
            self.scale_offset = 0;
            self.character_width = None;
            self.stack
                .push(StackEntry::new_sequence(&self.data_spec.root_descriptors));
            let subset_idx = self.current_subset_index;
            self.current_subset_index += 1;
            if self.data_spec.is_compressed {
                return Ok(DataEvent::CompressedStart);
            } else {
                return Ok(DataEvent::SubsetStart(subset_idx));
            }
        }
        self.process_next_descriptor()
    }

    fn process_next_descriptor(&mut self) -> Result<DataEvent, Error> {
        let top = self.stack.last_mut().expect("Stack should not be empty");
        if let StackEntryType::Replication { remaining, in_item } = &mut top.ty
            && top.next as usize >= top.descriptors.len()
        {
            if *in_item {
                *in_item = false;
                return Ok(DataEvent::ReplicationItemEnd);
            }
            if *remaining > 0 {
                *remaining -= 1;
                top.next = 0;
                *in_item = true;
                return Ok(DataEvent::ReplicationItemStart);
            } else {
                self.stack.pop();
                return Ok(DataEvent::ReplicationEnd);
            }
        };

        if top.next as usize >= top.descriptors.len() {
            self.stack.pop();
            return match (self.stack.last(), self.data_spec.is_compressed) {
                (Some(_), _) => Ok(DataEvent::SequenceEnd),
                (None, true) => Ok(DataEvent::Eof),
                (None, false) => Ok(DataEvent::SubsetEnd),
            };
        }

        let descriptors = &top.descriptors;
        let current_desc = &descriptors[top.next as usize];
        let idx = top.next;
        top.next += 1;
        match current_desc {
            ResolvedDescriptor::Data(b) => self.handle_data_descriptor(idx, b),
            ResolvedDescriptor::Replication {
                y,
                descriptors,
                delayed_bits,
            } => self.handle_replication_descriptor(idx, *y, descriptors, *delayed_bits),
            ResolvedDescriptor::Operator(xy) => self.handle_operator_descriptor(idx, *xy),
            ResolvedDescriptor::Sequence(d, elements) => {
                self.handle_sequence_descriptor(idx, d, elements)
            }
        }
    }

    // f = 0
    fn handle_data_descriptor(&mut self, idx: u16, b: &TableBEntry) -> Result<DataEvent, Error> {
        if b.unit == "CCITT IA5" {
            let width = self.character_width.unwrap_or(u32::from(b.bits));
            if !width.is_multiple_of(8) {
                return Err(Error::Invalid(
                    "Character width is not a whole number of bytes".into(),
                ));
            }
            let reference = self.read_string((width / 8) as usize)?;
            if self.data_spec.is_compressed {
                // The six-bit field is a BYTE count for strings, not a numeric
                // increment width. Zero repeats the reference for every subset.
                let bytes = self.reader.read::<6, u8>()?;
                let values = if bytes == 0 {
                    vec![reference; usize::from(self.data_spec.number_of_subsets)]
                } else {
                    (0..self.data_spec.number_of_subsets)
                        .map(|_| self.read_string(usize::from(bytes)))
                        .collect::<Result<Vec<_>, _>>()?
                };
                return Ok(DataEvent::CompressedData {
                    idx,
                    xy: b.xy,
                    values,
                });
            }
            return Ok(DataEvent::Data {
                idx,
                xy: b.xy,
                value: reference,
            });
        }
        // Width/scale operators do not apply to character, code or flag tables.
        let adjusted = !matches!(b.unit, "Code table" | "Flag table");
        let bit_width = i32::from(b.bits)
            + if adjusted {
                i32::from(self.width_offset)
            } else {
                0
            };
        if !(1..=64).contains(&bit_width) {
            return Err(Error::NotSupported(format!(
                "Numeric bit width {bit_width} is outside 1..=64"
            )));
        }
        let bit_width = bit_width as u32;
        let scale = i16::from(b.scale)
            + if adjusted {
                i16::from(self.scale_offset)
            } else {
                0
            };
        let value = |raw: u128| {
            let mantissa = raw as i128 + i128::from(b.reference_value);
            if scale == 0 {
                Value::Integer(mantissa)
            } else {
                Value::Decimal(mantissa, -scale)
            }
        };
        let reference: u64 = self.reader.read_var(bit_width)?;
        if self.data_spec.is_compressed {
            let increment_width = self.reader.read::<6, u8>()?;
            let values = if increment_width == 0 {
                let v = if reference == all_ones(bit_width) {
                    Value::Missing
                } else {
                    value(u128::from(reference))
                };
                vec![v; usize::from(self.data_spec.number_of_subsets)]
            } else {
                (0..self.data_spec.number_of_subsets)
                    .map(|_| {
                        let inc: u64 = self.reader.read_var(u32::from(increment_width))?;
                        // Missing is encoded in the increment's width, not by
                        // comparing reference+increment against the element width.
                        Ok(if inc == all_ones(u32::from(increment_width)) {
                            Value::Missing
                        } else {
                            value(u128::from(reference) + u128::from(inc))
                        })
                    })
                    .collect::<Result<Vec<_>, std::io::Error>>()?
            };
            Ok(DataEvent::CompressedData {
                idx,
                xy: b.xy,
                values,
            })
        } else {
            let value = if reference == all_ones(bit_width) {
                Value::Missing
            } else {
                value(u128::from(reference))
            };
            Ok(DataEvent::Data {
                idx,
                xy: b.xy,
                value,
            })
        }
    }

    fn read_string(&mut self, bytes: usize) -> Result<Value, Error> {
        let bytes = self.reader.read_to_vec(bytes)?;
        if bytes.iter().all(|&b| b == 0xff) {
            return Ok(Value::Missing);
        }
        String::from_utf8(bytes)
            .map(Value::String)
            .map_err(|_| Error::Invalid("Invalid character data".into()))
    }

    // f = 1
    fn handle_replication_descriptor(
        &mut self,
        idx: u16,
        y: u8,
        elements: &'a [ResolvedDescriptor<'_>],
        delayed_bits: u8,
    ) -> Result<DataEvent, Error> {
        let count = match y {
            0 => self.reader.read_var::<u16>(delayed_bits as u32)?,
            _ => y as u16,
        };
        self.stack
            .push(StackEntry::new_replication(elements, count));
        Ok(DataEvent::ReplicationStart { idx, count })
    }

    // f = 2
    fn handle_operator_descriptor(&mut self, idx: u16, xy: XY) -> Result<DataEvent, Error> {
        match (xy.x, xy.y) {
            // Change data width
            (1, 0) => self.width_offset = 0,
            (1, y) => self.width_offset = ((y as i16) - 128) as i8,
            // Change scale
            (2, 0) => self.scale_offset = 0,
            (2, y) => self.scale_offset = ((y as i16) - 128) as i8,
            // All local descriptors must already have resolved Table B entries.
            // Their widths come from those entries; 206 does not let this reader
            // skip unknown local descriptors (resolution rejects those first).
            (6, _) => {}
            // Change CCITT IA5 field width, in characters; zero cancels.
            (8, 0) => self.character_width = None,
            (8, y) => self.character_width = Some(u32::from(y) * 8),
            // Not supported
            _ => {
                return Err(Error::NotSupported(format!(
                    "Operator descriptor {xy:#?} not supported yet.",
                )));
            }
        }
        Ok(DataEvent::OperatorHandled {
            idx,
            x: xy.x,
            value: xy.y as i32,
        })
    }

    // f = 3
    fn handle_sequence_descriptor(
        &mut self,
        idx: u16,
        d: &TableDEntry,
        elements: &'a [ResolvedDescriptor<'_>],
    ) -> Result<DataEvent, Error> {
        self.stack.push(StackEntry::new_sequence(elements));
        Ok(DataEvent::SequenceStart { idx, xy: d.xy })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_fmt() {
        assert_eq!(format!("{:?}", Value::Missing), "Missing");
        assert_eq!(format!("{:?}", Value::Decimal(1234, -2)), "12.34");
        assert_eq!(format!("{:?}", Value::Decimal(1234, 2)), "123400");
        assert_eq!(format!("{:?}", Value::Integer(42)), "42");
        assert_eq!(
            format!("{:?}", Value::String("Hello".to_string())),
            "\"Hello\""
        );
    }
}

#[cfg(test)]
mod format_regressions {
    use super::*;
    use bitstream_io::{BitWrite, BitWriter};

    const NUM: TableBEntry = TableBEntry {
        xy: XY { x: 10, y: 4 },
        class_name: "test",
        element_name: "number",
        unit: "Pa",
        scale: 0,
        reference_value: -10,
        bits: 64,
    };
    const STRING: TableBEntry = TableBEntry {
        xy: XY { x: 1, y: 15 },
        class_name: "test",
        element_name: "string",
        unit: "CCITT IA5",
        scale: 0,
        reference_value: 0,
        bits: 8,
    };
    const CODE: TableBEntry = TableBEntry {
        unit: "Code table",
        bits: 8,
        reference_value: 0,
        ..NUM
    };
    const FLAG: TableBEntry = TableBEntry {
        unit: "Flag table",
        ..CODE
    };

    fn section(write: impl FnOnce(&mut BitWriter<Vec<u8>, BigEndian>)) -> Vec<u8> {
        let mut writer = BitWriter::endian(Vec::new(), BigEndian);
        write(&mut writer);
        writer.byte_align().unwrap();
        let payload = writer.into_writer();
        let len = u32::try_from(payload.len() + 4).unwrap().to_be_bytes();
        let mut bytes = vec![len[1], len[2], len[3], 0];
        bytes.extend(payload);
        bytes
    }

    fn values(bytes: &[u8], spec: &DataSpec<'_>) -> Result<Vec<Vec<Value>>, Error> {
        let mut reader = DataReader::new(bytes, spec)?;
        let mut out = Vec::new();
        loop {
            match reader.read_event()? {
                DataEvent::Data { value, .. } => out.push(vec![value]),
                DataEvent::CompressedData { values, .. } => out.push(values),
                DataEvent::Eof => return Ok(out),
                _ => {}
            }
        }
    }

    #[test]
    fn local_width_operator_requires_a_known_table_entry() {
        use crate::Descriptor;

        const LOCAL: TableBEntry = TableBEntry {
            xy: XY { x: 4, y: 214 },
            bits: 5,
            reference_value: 0,
            ..NUM
        };
        let descriptors = [
            Descriptor { f: 2, x: 6, y: 5 },
            Descriptor { f: 0, x: 4, y: 214 },
            Descriptor { f: 0, x: 10, y: 4 },
        ];
        let mut tables = Tables::default();
        tables.table_b.remove(&LOCAL.xy);
        assert!(matches!(
            resolve_descriptors(&tables, &descriptors),
            Err(Error::Table(_))
        ));
        tables.table_b.insert(LOCAL.xy, &LOCAL);
        tables.table_b.insert(NUM.xy, &NUM);
        let spec = DataSpec {
            number_of_subsets: 1,
            is_compressed: false,
            root_descriptors: resolve_descriptors(&tables, &descriptors).unwrap(),
        };
        let bytes = section(|w| {
            w.write::<5, u8>(17).unwrap();
            w.write::<64, u64>(42).unwrap();
        });
        assert_eq!(
            values(&bytes, &spec).unwrap(),
            vec![vec![Value::Integer(17)], vec![Value::Integer(32)]]
        );
    }

    #[test]
    fn full_u64_values_missing_and_wide_compressed_increments() {
        let mut spec = DataSpec {
            number_of_subsets: 2,
            is_compressed: false,
            root_descriptors: vec![ResolvedDescriptor::Data(&NUM)],
        };
        let bytes = section(|w| {
            w.write::<64, u64>(u64::MAX - 1).unwrap();
            w.write::<64, u64>(u64::MAX).unwrap();
        });
        assert_eq!(
            values(&bytes, &spec).unwrap(),
            vec![
                vec![Value::Integer(i128::from(u64::MAX) - 11)],
                vec![Value::Missing]
            ]
        );
        spec.is_compressed = true;
        let bytes = section(|w| {
            w.write::<64, u64>(u64::MAX - 100).unwrap();
            w.write::<6, u8>(40).unwrap();
            w.write::<40, u64>(1 << 35).unwrap();
            w.write::<40, u64>((1 << 40) - 1).unwrap();
        });
        assert_eq!(
            values(&bytes, &spec).unwrap(),
            vec![vec![
                Value::Integer(i128::from(u64::MAX) - 110 + (1i128 << 35)),
                Value::Missing
            ]]
        );
    }

    #[test]
    fn operator_width_cancellation_and_code_flag_exclusions() {
        let spec = DataSpec {
            number_of_subsets: 2,
            is_compressed: false,
            root_descriptors: vec![
                ResolvedDescriptor::Data(&STRING),
                ResolvedDescriptor::Operator(XY { x: 8, y: 3 }),
                ResolvedDescriptor::Data(&STRING),
                ResolvedDescriptor::Operator(XY { x: 8, y: 0 }),
                ResolvedDescriptor::Data(&STRING),
                ResolvedDescriptor::Operator(XY { x: 1, y: 160 }),
                ResolvedDescriptor::Operator(XY { x: 2, y: 130 }),
                ResolvedDescriptor::Data(&CODE),
                ResolvedDescriptor::Data(&FLAG),
                // No cancellation: state must reset for the next subset.
                ResolvedDescriptor::Operator(XY { x: 8, y: 7 }),
            ],
        };
        let bytes = section(|w| {
            for _ in 0..2 {
                w.write_bytes(b"AABCZ").unwrap();
                w.write::<8, u8>(7).unwrap();
                w.write::<8, u8>(8).unwrap();
            }
        });
        let row = vec![
            vec![Value::String("A".into())],
            vec![Value::String("ABC".into())],
            vec![Value::String("Z".into())],
            vec![Value::Integer(7)],
            vec![Value::Integer(8)],
        ];
        assert_eq!(values(&bytes, &spec).unwrap(), [row.clone(), row].concat());
    }

    #[test]
    fn compressed_missing_strings_consume_bytes_and_preserve_alignment() {
        let spec = DataSpec {
            number_of_subsets: 2,
            is_compressed: true,
            root_descriptors: vec![
                ResolvedDescriptor::Data(&STRING),
                ResolvedDescriptor::Data(&CODE),
            ],
        };
        for constant in [false, true] {
            let bytes = section(|w| {
                w.write::<8, u8>(255).unwrap();
                w.write::<6, u8>(if constant { 0 } else { 1 }).unwrap();
                if !constant {
                    w.write::<8, u8>(255).unwrap();
                    w.write::<8, u8>(b'B').unwrap();
                }
                w.write::<8, u8>(7).unwrap();
                w.write::<6, u8>(0).unwrap();
            });
            assert_eq!(
                values(&bytes, &spec).unwrap(),
                vec![
                    vec![
                        Value::Missing,
                        if constant {
                            Value::Missing
                        } else {
                            Value::String("B".into())
                        }
                    ],
                    vec![Value::Integer(7), Value::Integer(7)],
                ]
            );
        }
    }

    #[test]
    fn truncated_section_cannot_consume_end_marker_as_data() {
        let spec = DataSpec {
            number_of_subsets: 1,
            is_compressed: false,
            root_descriptors: vec![ResolvedDescriptor::Data(&CODE)],
        };
        assert!(values(b"\0\0\x04\0\x37777", &spec).is_err());
        assert!(values(b"\0\0\x03\0", &spec).is_err());
    }
}
