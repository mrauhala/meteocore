#![no_main]

use engine_geotiff::fuzz_exports::{DataSource, TiffMetadata};
use libfuzzer_sys::fuzz_target;

// Fuzz the TIFF metadata parser with arbitrary bytes.
// Exercises IFD parsing, GeoKey extraction, CRS detection,
// tile layout validation, and all security limit checks.
fuzz_target!(|data: &[u8]| {
    let owned = data.to_vec();
    let source = DataSource::from_bytes(owned);
    let _ = TiffMetadata::from_source(&source);
});
