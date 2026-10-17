//! The engine-wide cache of decoded fields, and the one place a request
//! reads a file.
//!
//! Every field read goes through [`FIELDS`], keyed by the file's ingest,
//! the variable and the time step. Its single-flight guard makes N
//! concurrent renders of one cold field decode it once: the first reader
//! owns the fill and reserves the transient peak from the decode budget,
//! the others wait for its value and reserve nothing, and a hit reserves
//! nothing either. The wait and the fill run inside `block_in_place` on a
//! Tokio worker, as engine-zarr's store does (root CLAUDE.md Critical
//! Rule 7), so a waiting async worker does not hold up the runtime.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use ds_cache::ByteBoundedCache;
use ds_core::deadline;
use ds_core::error::DataServerError;
use netcdf_reader::NcFile;

use crate::decode_budget::BUDGET;
use crate::ingest::{FieldRead, Stored};

/// A decoded 2-D field: rows north to south, NaN for nodata.
pub(crate) type Values = Arc<[f32]>;

/// What identifies a decoded field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FieldKey {
    /// The ingest that opened the file ([`crate::catalog::OpenFile::id`]),
    /// unique in the process: a file replaced on disk is a new ingest, and
    /// never reads the fields of the old one.
    pub file: u64,
    pub variable: Arc<str>,
    pub time: u32,
    /// The index along a vertical axis. Always 0 while a field has a single
    /// level.
    pub level: u32,
}

/// A MEPS field (949 × 1069 `f32`) is 4.06 MB.
const TYPICAL_FIELD_BYTES: u64 = 4 * ds_cache::MIB;

fn weigh(key: &FieldKey, values: &Values) -> u64 {
    std::mem::size_of_val::<[f32]>(values) as u64 + key.variable.len() as u64 + 96
}

/// Decoded fields, `MC_NETCDF_FIELD_CACHE_MB` (default 256 MiB), shared by
/// every NetCDF collection.
static FIELDS: LazyLock<ByteBoundedCache<FieldKey, Values>> = LazyLock::new(|| {
    ByteBoundedCache::from_env("MC_NETCDF_FIELD_CACHE_MB", 256, TYPICAL_FIELD_BYTES, weigh)
});

/// The field cache's counters and occupancy.
pub fn field_cache_metrics() -> ds_cache::CacheMetrics {
    FIELDS.metrics()
}

/// The transient peak of decoding `read`, which the fill reserves: three
/// times the decoded bytes (the stored-type result, the inflated chunk and
/// its unshuffled copy) and three times the compressed chunk. hdf5-reader
/// exposes no chunk's stored size before reading it, so the decoded chunk
/// size bounds the compressed one: about 24 MB for a MEPS field.
fn peak_bytes(read: &FieldRead) -> u64 {
    let decoded = read.field_bytes.max(read.chunk_bytes);
    decoded
        .saturating_mul(3)
        .saturating_add(read.chunk_bytes.saturating_mul(3))
}

/// The field `read` of `nc` at time step `time`, from the cache or decoded
/// once for every concurrent reader. `file` is the file's ingest id
/// ([`FieldKey::file`]); `flipped` reverses the stored rows to north-up;
/// `decodes` counts the fills this call makes.
pub(crate) fn read(
    nc: &NcFile,
    file: u64,
    read: &FieldRead,
    time: u32,
    flipped: bool,
    decodes: &AtomicU64,
) -> Result<Values, DataServerError> {
    deadline::check()?;
    let key = FieldKey {
        file,
        variable: read.name.clone(),
        time,
        level: 0,
    };
    if let Some(values) = FIELDS.get_untracked(&key) {
        FIELDS.record_hit();
        return Ok(values);
    }
    let end = deadline::current();
    let wait = end.map_or(Duration::from_secs(30), |end| {
        end.saturating_duration_since(Instant::now())
    });
    let fill = || {
        FIELDS.get_or_insert_with_timeout(
            &key,
            wait,
            || {
                // A failed owner hands the fill to a waiter: recheck that
                // caller's deadline before reading.
                deadline::check()?;
                let _permit = BUDGET.reserve(peak_bytes(read))?;
                decodes.fetch_add(1, Ordering::Relaxed);
                decode(nc, read, time, flipped)
                    .map(Values::from)
                    .map_err(DataServerError::Storage)
            },
            || {
                if end.is_some() {
                    DataServerError::DeadlineExceeded
                } else {
                    DataServerError::Storage("timed out waiting for a NetCDF field read".into())
                }
            },
        )
    };
    // A waiting async worker must not hold up the runtime the owner may
    // need. On RenderJob's blocking threads `block_in_place` runs the
    // closure as is.
    let result = if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(fill)
    } else {
        fill()
    };
    // A fill can complete after this caller's deadline passed: the value
    // stays cached for the next request.
    deadline::check()?;
    result
}

/// Read and decode one field: the typed hyperslab, CF masking and packing,
/// and the rows reversed to north-up when the file stores them south-up.
fn decode(nc: &NcFile, read: &FieldRead, time: u32, flipped: bool) -> Result<Vec<f32>, String> {
    let selection = read.selection(time);
    let name = &*read.name;
    let packing = &read.packing;
    let (ny, nx) = read.shape();
    let cells = ny * nx;
    let fail = |e: netcdf_reader::Error| format!("variable '{name}' step {time}: {e}");
    macro_rules! typed {
        ($t:ty) => {{
            let array = nc
                .read_variable_slice::<$t>(name, &selection)
                .map_err(fail)?;
            let mut out = Vec::with_capacity(cells);
            match array.as_slice() {
                Some(values) if values.len() == cells => {
                    if flipped {
                        for row in values.chunks_exact(nx).rev() {
                            out.extend(row.iter().map(|&v| packing.decode(v as f64)));
                        }
                    } else {
                        out.extend(values.iter().map(|&v| packing.decode(v as f64)));
                    }
                }
                _ => {
                    out.extend(array.iter().map(|&v| packing.decode(v as f64)));
                    if out.len() == cells && flipped {
                        flip_rows(&mut out, nx);
                    }
                }
            }
            out
        }};
    }
    let values = match read.stored {
        Stored::Byte => typed!(i8),
        Stored::UByte => typed!(u8),
        Stored::Short => typed!(i16),
        Stored::UShort => typed!(u16),
        Stored::Int => typed!(i32),
        Stored::UInt => typed!(u32),
        Stored::Float => typed!(f32),
        Stored::Double => typed!(f64),
    };
    if values.len() != cells {
        return Err(format!(
            "variable '{name}' step {time}: read {} values for a {nx} x {ny} field",
            values.len()
        ));
    }
    Ok(values)
}

/// Reverse the rows of a row-major field `nx` wide, in place.
fn flip_rows(values: &mut [f32], nx: usize) {
    let ny = values.len() / nx;
    for top in 0..ny / 2 {
        let bottom = ny - 1 - top;
        let (upper, lower) = values.split_at_mut(bottom * nx);
        upper[top * nx..(top + 1) * nx].swap_with_slice(&mut lower[..nx]);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use netcdf_reader::{DynStorage, FileStorage, Storage, StorageBuffer};

    use super::*;
    use crate::ingest::{inspect, open_storage, Wanted};

    /// File storage whose reads take `delay` once `slow` is set.
    struct Slow {
        inner: FileStorage,
        slow: std::sync::atomic::AtomicBool,
        reads: AtomicU64,
    }

    impl Storage for Slow {
        fn len(&self) -> u64 {
            self.inner.len()
        }

        fn read_range(&self, offset: u64, len: usize) -> hdf5_reader::error::Result<StorageBuffer> {
            if self.slow.load(Ordering::Relaxed) {
                self.reads.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(20));
            }
            self.inner.read_range(offset, len)
        }
    }

    /// Eight readers of one cold field arrive while the first decodes it
    /// (each storage read takes 20 ms): one decode, one set of reads, one
    /// shared value.
    #[test]
    fn concurrent_readers_of_a_cold_field_share_one_decode() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/meps-netcdf/meps_sfc_01_20261015T06Z.nc");
        let storage = Arc::new(Slow {
            inner: FileStorage::open(path).unwrap(),
            slow: false.into(),
            reads: AtomicU64::new(0),
        });
        let dynamic: DynStorage = storage.clone();
        let nc = open_storage(dynamic.clone()).unwrap();
        let names = vec!["wind_speed_of_gust".to_string()];
        let inspected = inspect(&nc, &dynamic, &Wanted::Listed(&names), 0).unwrap();
        let field = &inspected.fields[0].read;
        storage.slow.store(true, Ordering::Relaxed);
        let (file, decodes) = (crate::next_file_id(), AtomicU64::new(0));
        let start = Barrier::new(8);
        let values: Vec<Values> = std::thread::scope(|scope| {
            let readers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        read(&nc, file, field, 0, inspected.grid.flipped, &decodes).unwrap()
                    })
                })
                .collect();
            readers.into_iter().map(|r| r.join().unwrap()).collect()
        });
        assert_eq!(decodes.load(Ordering::Relaxed), 1);
        let reads = storage.reads.load(Ordering::Relaxed);
        assert!(
            (1..=9).contains(&reads),
            "{reads} storage reads for one decode"
        );
        assert!(values.iter().all(|v| Arc::ptr_eq(v, &values[0])));
        assert_eq!(values[0].len(), 110);
    }

    #[test]
    fn flip_rows_reverses_rows_in_place() {
        let mut values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        flip_rows(&mut values, 2);
        assert_eq!(values, [5.0, 6.0, 3.0, 4.0, 1.0, 2.0]);
        let mut even = vec![1.0, 2.0, 3.0, 4.0];
        flip_rows(&mut even, 1);
        assert_eq!(even, [4.0, 3.0, 2.0, 1.0]);
    }
}
