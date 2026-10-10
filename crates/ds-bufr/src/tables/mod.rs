//! The BUFR edition 4 tables

mod corrections;
pub mod local;
mod table_b;
mod table_c;
mod table_d;

pub use table_b::*;
pub use table_c::*;
pub use table_d::*;

use crate::{Descriptor, XY};
use hashbrown::HashMap;

/// Collection of BUFR tables (B, C, D).
#[derive(Clone)]
pub struct Tables {
    pub table_b: HashMap<XY, &'static TableBEntry>,
    pub table_c: HashMap<(u8, Option<u8>), &'static TableCEntry>,
    pub table_d: HashMap<XY, &'static TableDEntry>,
}

impl Default for Tables {
    fn default() -> Self {
        Self {
            table_b: make_table_b(),
            table_c: make_table_c(),
            table_d: make_table_d(),
        }
    }
}

/// Entry in Table B (element descriptors).
#[derive(Debug)]
pub struct TableBEntry {
    pub xy: XY,
    pub class_name: &'static str,
    pub element_name: &'static str,
    pub unit: &'static str,
    pub scale: i8,
    pub reference_value: i32,
    pub bits: u16,
}

/// Entry in Table C (operator descriptors).
#[derive(Debug)]
pub struct TableCEntry {
    pub xy: (u8, Option<u8>),
    pub operator_name: &'static str,
    pub operation_definition: &'static str,
}

/// Entry in Table D (sequence descriptors).
#[derive(Debug)]
pub struct TableDEntry {
    pub xy: XY,
    pub category: &'static str,
    pub title: &'static str,
    pub sub_title: &'static str,
    pub elements: &'static [Descriptor],
}

/// Table B (f = 0).
fn make_table_b() -> HashMap<XY, &'static TableBEntry> {
    let mut map = HashMap::new();
    for entry in &table_b::TABLE_B {
        map.insert(entry.xy, entry);
    }
    map
}

/// Table C (f = 2).
fn make_table_c() -> HashMap<(u8, Option<u8>), &'static TableCEntry> {
    let mut map = HashMap::new();
    for entry in &table_c::TABLE_C {
        map.insert(entry.xy, entry);
    }
    map
}

/// Table D (f = 3).
fn make_table_d() -> HashMap<XY, &'static TableDEntry> {
    let mut map = HashMap::new();
    for entry in &table_d::TABLE_D {
        map.insert(entry.xy, entry);
    }
    for entry in &corrections::TABLE_D {
        map.insert(entry.xy, entry);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence(tables: &Tables, y: u8) -> Vec<(u8, u8, u8)> {
        tables.table_d[&XY { x: 7, y }]
            .elements
            .iter()
            .map(|d| (d.f, d.x, d.y))
            .collect()
    }

    #[test]
    fn synop_ra_sequences_carry_their_identification_and_pressure_groups() {
        // The generated table dropped `301090 302031` from 307083 and lost
        // 307082 (#1008); both start like 307080 and its RA siblings.
        let tables = Tables::default();
        for y in [80, 81, 82, 83, 84, 86] {
            assert_eq!(
                sequence(&tables, y)[..2],
                [(3, 1, 90), (3, 2, 31)],
                "3-07-0{y}"
            );
        }
        assert_eq!(sequence(&tables, 82).len(), 15);
        assert_eq!(sequence(&tables, 83).len(), 14);
    }
}
