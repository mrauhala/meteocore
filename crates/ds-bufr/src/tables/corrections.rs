//! Corrections to the generated WMO Table D (#1008).
//!
//! The generated `table_d.rs` (inherited unchanged from tinybufr 0.1.3) lost
//! the leading `301090 302031` of 307083 and omits 307082 entirely, so a
//! SYNOP encoded with either template misreads its whole data section (live:
//! bb-barbadosmetservices, 307083). The generated file stays untouched; these
//! entries replace or add theirs when the tables are built. Element lists are
//! the WMO sequences as ecCodes 2.47.0 ships them in
//! `bufr/tables/0/wmo/<version>/sequence.def`, identical in every master
//! version from 7 through 45.

use super::{TableDEntry, XY};
use crate::Descriptor;

const fn d(f: u8, x: u8, y: u8) -> Descriptor {
    Descriptor { f, x, y }
}

pub static TABLE_D: [TableDEntry; 2] = [
    TableDEntry {
        xy: XY { x: 7, y: 82 },
        category: "Surface report sequences (land)",
        title: "Sequence for representation of synoptic reports from a fixed land station suitable for SYNOP data in compliance with reporting practices in RA II",
        sub_title: "",
        elements: &[
            d(3, 1, 90),
            d(3, 2, 31),
            d(3, 2, 35),
            d(3, 2, 36),
            d(3, 2, 47),
            d(0, 8, 2),
            d(3, 2, 48),
            d(3, 2, 37),
            d(0, 12, 121),
            d(0, 12, 122),
            d(3, 2, 43),
            d(3, 2, 44),
            d(1, 1, 2),
            d(3, 2, 45),
            d(3, 2, 46),
        ],
    },
    TableDEntry {
        xy: XY { x: 7, y: 83 },
        category: "Surface report sequences (land)",
        title: "Sequence for representation of synoptic reports from a fixed land station suitable for SYNOP data in compliance with reporting practices in RA III",
        sub_title: "",
        elements: &[
            d(3, 1, 90),
            d(3, 2, 31),
            d(3, 2, 35),
            d(3, 2, 36),
            d(3, 2, 47),
            d(0, 8, 2),
            d(3, 2, 48),
            d(3, 2, 37),
            d(0, 12, 122),
            d(3, 2, 43),
            d(3, 2, 44),
            d(1, 1, 2),
            d(3, 2, 45),
            d(3, 2, 46),
        ],
    },
];
