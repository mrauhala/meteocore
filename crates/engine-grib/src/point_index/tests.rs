//! The point-index cache itself (#1302).

use super::*;
use crate::test_support::{Complex, Value};

fn key(step: usize) -> MessageKey {
    let entry = MessageEntry {
        source_url: None,
        param: "TMP".into(),
        levtype: "hag".into(),
        level: Some(2),
        offset: 0,
        length: Some(1),
        step_kind: crate::wgrib2_index::StepKind::Instant,
    };
    MessageKey::new(&format!("gfs.t00z.pgrb2.0p25.f{step:03}"), &entry)
}

/// One shard, so a budget that holds a window's indexes keeps them all:
/// 16 MiB holds a five-day hourly window of 121 GFS-sized 5.3 indexes, none
/// evicted. Sharded from the old 4 KiB hint it was 64 slices of 256 KiB, two
/// such indexes each, and kept 86 of the 121.
#[test]
fn point_index_is_one_shard_so_a_window_of_gfs_indexes_stays_resident() {
    let index = PointIndex::new(16).unwrap();
    assert_eq!(
        index.cache.shard_capacity_bytes(),
        index.cache.capacity_bytes()
    );
    // A GFS 0.25° field: 1440 × 721 nodes in groups of 3 to 60 values.
    let spec = Complex {
        ni: 1440,
        nj: 721,
        step: 250_000,
        lengths: vec![14, 37, 3, 52, 25, 60, 9, 31],
        ..Complex::global()
    };
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let values: Vec<Value> = (0..1440 * 721)
        .map(|k: i64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            Value::Present(20_000 + (k % 1440 - 720).abs() * 3 + (seed % 61) as i64)
        })
        .collect();
    let message = spec.message(&values);
    let (built, _) = complex::Header::parse(&message)
        .unwrap()
        .index(&message, &[])
        .unwrap();
    let built = Arc::new(built);
    // GFS indexes measure 97 to 118 KB, 721 rows and the descriptors.
    let size = weight(&key(0), &Index::Complex(built.clone()));
    assert!((90_000..130_000).contains(&size), "{size}");
    assert_eq!(built.indexed_rows(), 721);

    let keys: Vec<MessageKey> = (0..121).map(key).collect();
    for key in &keys {
        index.store_complex(key, built.clone());
    }
    assert!(keys
        .iter()
        .all(|key| index.cache.get_untracked(key).is_some()));
    assert_eq!(index.cache.len(), 121);
}
