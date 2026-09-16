//! The header sections of BUFR files

use std::io::Read;

use byteorder::{BigEndian, ReadBytesExt};

use crate::{Descriptor, Error, reader::three_bytes_to_u32};

/// The header sections of a BUFR file.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct HeaderSections {
    pub indicator_section: IndicatorSection,
    pub identification_section: IdentificationSection,
    pub optional_section: Option<OptionalSection>,
    pub data_description_section: DataDescriptionSection,
}

impl HeaderSections {
    pub fn read<R: Read>(mut reader: R) -> Result<Self, Error> {
        // Indicator section
        let indicator_section = IndicatorSection::read(&mut reader)?;

        // Identification section
        let identification_section = match indicator_section.edition_number {
            3 => IdentificationSectionV3::read(&mut reader)?.into(),
            4 => IdentificationSection::read(&mut reader)?,
            _ => {
                return Err(Error::Invalid(format!(
                    "Unsupported edition number {}",
                    indicator_section.edition_number
                )));
            }
        };

        // Optional section
        let optional_section = match identification_section.flags.has_optional_section {
            true => Some(OptionalSection::read(&mut reader)?),
            false => None,
        };

        // Data description section
        let data_description_section = DataDescriptionSection::read(&mut reader)?;

        Ok(HeaderSections {
            indicator_section,
            identification_section,
            optional_section,
            data_description_section,
        })
    }
}

/// Indicator section (Section 0).
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct IndicatorSection {
    pub total_length: u32,
    pub edition_number: u8,
}

impl IndicatorSection {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if &magic != b"BUFR" {
            return Err(Error::Invalid("Invalid magic number".to_string()));
        }

        let mut len_bytes = [0u8; 3];
        reader.read_exact(&mut len_bytes)?;
        let total_length = three_bytes_to_u32(len_bytes);

        let edition_number = reader.read_u8()?;

        Ok(Self {
            total_length,
            edition_number,
        })
    }
}

/// Identification section (Section 1) for BUFR edition 4.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct IdentificationSection {
    pub section_length: u32,
    pub master_table_number: u8,
    pub centre: u16,
    pub sub_centre: u16,
    pub update_sequence_number: u8,
    pub flags: IdentificationSectionFlags,
    pub data_category: u8,
    pub international_data_sub_category: u8,
    pub local_data_sub_category: u8,
    pub master_table_version: u8,
    pub local_tables_version: u8,
    pub typical_year: u16,
    pub typical_month: u8,
    pub typical_day: u8,
    pub typical_hour: u8,
    pub typical_minute: u8,
    pub typical_second: u8,
    pub local_use: Vec<u8>,
}

impl IdentificationSection {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let mut len_bytes = [0u8; 3];
        reader.read_exact(&mut len_bytes)?;
        let section_length = three_bytes_to_u32(len_bytes);

        if section_length < 22 {
            return Err(Error::Invalid(
                "Identification section (BUFR4) length must be >= 22".to_string(),
            ));
        }

        let master_table_number = reader.read_u8()?;
        let centre = reader.read_u16::<BigEndian>()?;
        let sub_centre = reader.read_u16::<BigEndian>()?;
        let update_sequence_number = reader.read_u8()?;
        let flags = IdentificationSectionFlags::read(reader)?;
        let data_category = reader.read_u8()?;
        let international_data_sub_category = reader.read_u8()?;
        let local_data_sub_category = reader.read_u8()?;
        let master_table_version = reader.read_u8()?;
        let local_tables_version = reader.read_u8()?;
        let typical_year = reader.read_u16::<BigEndian>()?;
        let typical_month = reader.read_u8()?;
        let typical_day = reader.read_u8()?;
        let typical_hour = reader.read_u8()?;
        let typical_minute = reader.read_u8()?;
        let typical_second = reader.read_u8()?;

        let mut local_use = vec![0u8; (section_length - 22) as usize];
        reader.read_exact(&mut local_use)?;

        Ok(Self {
            section_length,
            master_table_number,
            centre,
            sub_centre,
            update_sequence_number,
            flags,
            data_category,
            international_data_sub_category,
            local_data_sub_category,
            master_table_version,
            local_tables_version,
            typical_year,
            typical_month,
            typical_day,
            typical_hour,
            typical_minute,
            typical_second,
            local_use,
        })
    }
}

/// Identification section for BUFR edition 3.
#[derive(Debug)]
pub struct IdentificationSectionV3 {
    pub section_length: u32,
    pub master_table_number: u8,
    pub sub_centre: u8,
    pub centre: u8,
    pub update_sequence_number: u8,
    pub flags: IdentificationSectionFlags,
    pub data_category: u8,
    pub data_sub_category: u8,
    pub master_table_version: u8,
    pub local_tables_version: u8,
    pub typical_year: u8,
    pub typical_month: u8,
    pub typical_day: u8,
    pub typical_hour: u8,
    pub typical_minute: u8,
    pub local_use: Vec<u8>,
}

impl IdentificationSectionV3 {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let mut len_bytes = [0u8; 3];
        reader.read_exact(&mut len_bytes)?;
        let section_length = three_bytes_to_u32(len_bytes);

        if section_length < 17 {
            return Err(Error::Invalid(
                "Identification section (BUFR3) length must be >= 17".to_string(),
            ));
        }

        let master_table_number = reader.read_u8()?;
        let sub_centre = reader.read_u8()?;
        let centre = reader.read_u8()?;
        let update_sequence_number = reader.read_u8()?;
        let flags = IdentificationSectionFlags::read(reader)?;
        let data_category = reader.read_u8()?;
        let data_sub_category = reader.read_u8()?;
        let master_table_version = reader.read_u8()?;
        let local_tables_version = reader.read_u8()?;
        let typical_year = reader.read_u8()?;
        let typical_month = reader.read_u8()?;
        let typical_day = reader.read_u8()?;
        let typical_hour = reader.read_u8()?;
        let typical_minute = reader.read_u8()?;

        let mut local_use = vec![0u8; (section_length - 17) as usize];
        reader.read_exact(&mut local_use)?;

        Ok(Self {
            section_length,
            master_table_number,
            sub_centre,
            centre,
            update_sequence_number,
            flags,
            data_category,
            data_sub_category,
            master_table_version,
            local_tables_version,
            typical_year,
            typical_month,
            typical_day,
            typical_hour,
            typical_minute,
            local_use,
        })
    }
}

impl From<IdentificationSectionV3> for IdentificationSection {
    fn from(value: IdentificationSectionV3) -> Self {
        Self {
            section_length: value.section_length,
            master_table_number: value.master_table_number,
            centre: value.centre as u16,
            sub_centre: value.sub_centre as u16,
            update_sequence_number: value.update_sequence_number,
            flags: value.flags,
            data_category: value.data_category,
            international_data_sub_category: value.data_sub_category,
            local_data_sub_category: 0,
            master_table_version: value.master_table_version,
            local_tables_version: value.local_tables_version,
            typical_year: value.typical_year as u16,
            typical_month: value.typical_month,
            typical_day: value.typical_day,
            typical_hour: value.typical_hour,
            typical_minute: value.typical_minute,
            typical_second: 0,
            local_use: value.local_use,
        }
    }
}

/// Flags in the identification section.
#[derive(Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct IdentificationSectionFlags {
    pub has_optional_section: bool,
}

impl IdentificationSectionFlags {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let flags = reader.read_u8()?;
        Ok(Self {
            has_optional_section: flags & 0b10000000 != 0,
        })
    }
}

/// Optional section (Section 2).
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OptionalSection {
    pub section_length: u32,
    pub optional: Vec<u8>,
}

impl OptionalSection {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let mut len_bytes = [0u8; 3];
        reader.read_exact(&mut len_bytes)?;
        let section_length = three_bytes_to_u32(len_bytes);

        // Skip reserved byte
        reader.read_u8()?;

        if section_length < 4 {
            return Err(Error::Invalid(
                "Optional section length must be >= 4".to_string(),
            ));
        }

        let mut optional = vec![0u8; (section_length - 4) as usize];
        reader.read_exact(&mut optional)?;

        Ok(Self {
            section_length,
            optional,
        })
    }
}

/// Data description section (Section 3).
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct DataDescriptionSection {
    pub section_length: u32,
    pub number_of_subsets: u16,
    pub flags: DataDescriptionSectionFlags,
    pub descriptors: Vec<Descriptor>,
    pub _padding: Vec<u8>,
}

impl DataDescriptionSection {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let mut len_bytes = [0u8; 3];
        reader.read_exact(&mut len_bytes)?;
        let section_length = three_bytes_to_u32(len_bytes);

        // Skip reserved byte
        reader.read_u8()?;

        if section_length < 7 {
            return Err(Error::Invalid(
                "Data description section length must be >= 7".to_string(),
            ));
        }

        let number_of_subsets = reader.read_u16::<BigEndian>()?;
        let flags = DataDescriptionSectionFlags::read(reader)?;

        let descriptor_count = ((section_length - 7) / 2) as usize;
        let mut descriptors = Vec::with_capacity(descriptor_count);

        for _ in 0..descriptor_count {
            descriptors.push(Descriptor::read(reader)?);
        }

        let padding_len = section_length as usize - 7 - (2 * descriptors.len());
        let mut padding = vec![0u8; padding_len];
        reader.read_exact(&mut padding)?;

        Ok(Self {
            section_length,
            number_of_subsets,
            flags,
            descriptors,
            _padding: padding,
        })
    }
}

/// Flags in the data description section.
#[derive(Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct DataDescriptionSectionFlags {
    pub is_observed_data: bool,
    pub is_compressed: bool,
}

impl DataDescriptionSectionFlags {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let flags = reader.read_u8()?;
        Ok(Self {
            is_observed_data: flags & 0b10000000 != 0,
            is_compressed: flags & 0b01000000 != 0,
        })
    }
}

/// The header of the data section (Section 4).
#[derive(Debug)]
pub struct DataSectionHeader {
    pub section_length: u32,
}

impl DataSectionHeader {
    pub fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let mut len_bytes = [0u8; 3];
        reader.read_exact(&mut len_bytes)?;
        let section_length = three_bytes_to_u32(len_bytes);

        // Skip reserved byte
        reader.read_u8()?;

        Ok(Self { section_length })
    }
}

/// End section (Section 5).
#[derive(Debug)]
pub struct EndSection {}

/// Check if the end section appears in the stream.
pub fn ensure_end_section<R: std::io::Read>(edition: u8, reader: &mut R) -> Result<(), Error> {
    if edition == 3 {
        let mut buf: [u8; 1] = [0; 1];
        reader.read_exact(&mut buf)?;
        match buf[0] {
            0x0 => {}
            b'7' => {
                let mut buf: [u8; 3] = [0; 3];
                reader.read_exact(&mut buf)?;
                if &buf != b"777" {
                    return Err(Error::Invalid("Invalid end section".to_string()));
                }
            }
            _ => {
                return Err(Error::Invalid("Invalid end section".to_string()));
            }
        }
    }
    let mut buf: [u8; 4] = [0; 4];
    reader.read_exact(&mut buf)?;
    if &buf != b"7777" {
        return Err(Error::Invalid("Invalid end section".to_string()));
    }
    Ok(())
}
