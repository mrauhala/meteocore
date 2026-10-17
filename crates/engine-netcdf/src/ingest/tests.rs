use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use netcdf_reader::{Storage, StorageBuffer};

use super::*;

fn fixture(path: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(path)
}

const MEPS_00: &str = "meps-netcdf/meps_sfc_00_20261015T06Z.nc";

/// A storage counting its reads and bytes.
struct Counting {
    inner: DynStorage,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl Counting {
    fn take(&self) -> (u64, u64) {
        (
            self.reads.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed),
        )
    }
}

impl Storage for Counting {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_range(&self, offset: u64, len: usize) -> hdf5_reader::error::Result<StorageBuffer> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(len as u64, Ordering::Relaxed);
        self.inner.read_range(offset, len)
    }
}

fn counting(path: &str) -> Arc<Counting> {
    Arc::new(Counting {
        inner: Arc::new(FileStorage::open(fixture(path)).unwrap()),
        reads: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    })
}

fn open_fixture(path: &str) -> (NcFile, DynStorage) {
    open(&fixture(path)).unwrap()
}

/// What a request's read costs once a file is ingested, measured on the
/// MEPS fixture (8 served variables of 18): ingest builds the one metadata
/// tree it reads, `variables()`, and warms each served variable's dataset
/// lookup and chunk index, so the first field read reads the index and the
/// chunk (3 reads, against 9 unwarmed) and a repeat the chunk alone.
/// `variable(name)` would build a second tree: 175 more reads.
#[test]
fn an_ingested_field_reads_only_its_index_and_chunk() {
    let mut first_reads = Vec::new();
    for warm in [false, true] {
        let storage = counting(MEPS_00);
        let dynamic: DynStorage = storage.clone();
        let nc = open_storage(dynamic.clone()).unwrap();
        let names = vec!["air_temperature_2m".to_string()];
        let inspected = inspect(&nc, &dynamic, &Wanted::Listed(&names), 0).unwrap();
        if warm {
            for field in &inspected.fields {
                nc.iter_variable_chunks(&field.read.name).unwrap();
            }
        }
        storage.take();
        let read = &inspected.fields[0].read;
        nc.read_variable_slice::<f32>(&read.name, &read.selection(0))
            .unwrap();
        let (first, _) = storage.take();
        nc.read_variable_slice::<f32>(&read.name, &read.selection(0))
            .unwrap();
        assert_eq!(storage.take().0, 1, "warm={warm}: a repeat reads the chunk");
        nc.variables().unwrap();
        assert_eq!(storage.take(), (0, 0), "the metadata tree is kept");
        first_reads.push(first);
    }
    assert_eq!(first_reads, [9, 3], "unwarmed, warmed");
}

#[test]
fn meps_fields_are_lambert_south_up_with_levels_by_value() {
    let (nc, storage) = open_fixture(MEPS_00);
    let inspected = inspect(&nc, &storage, &Wanted::All, 0).unwrap();
    assert_eq!(inspected.fields.len(), 8, "{:?}", inspected.skipped);
    // The 2-D latitude/longitude are not on the grid's time axis.
    let mut skipped: Vec<&str> = inspected.skipped.iter().map(|(n, _)| n.as_str()).collect();
    skipped.sort_unstable();
    assert_eq!(
        skipped,
        ["latitude", "longitude"],
        "{:?}",
        inspected.skipped
    );
    let grid = &inspected.grid;
    assert!(grid.flipped, "MEPS's y ascends");
    assert!(matches!(
        grid.transform.crs,
        Crs::LambertConformalConic { radius: Some(r), .. } if r == 6_371_000.0
    ));
    assert_eq!((grid.transform.width, grid.transform.height), (10, 11));
    let run = DateTime::parse_from_rfc3339("2026-10-15T06:00:00Z").unwrap();
    assert_eq!(inspected.run, Some(run.into()));
    assert_eq!(inspected.times, [DateTime::<Utc>::from(run)]);
    let field = |name: &str| {
        inspected
            .fields
            .iter()
            .find(|f| &*f.variable.name == name)
            .unwrap()
    };
    let t2m = field("air_temperature_2m");
    assert_eq!(t2m.variable.units, "K");
    assert_eq!(t2m.variable.title, "Screen level temperature (T2M)");
    assert_eq!(
        t2m.variable.levels,
        [Level {
            value: 2.0,
            units: "m".into()
        }]
    );
    assert_eq!(field("x_wind_10m").variable.levels[0].value, 10.0);
    let read = &t2m.read;
    assert_eq!(read.stored, Stored::Float);
    assert_eq!(
        read.dims,
        [Dim::Time, Dim::Index(0), Dim::Whole(11), Dim::Whole(10)]
    );
    assert_eq!(read.packing.fill, [9.96921e36_f32 as f64]);
    assert_eq!((read.field_bytes, read.chunk_bytes), (440, 440));
}

#[test]
fn a_global_grid_north_up_without_runs_with_packing() {
    let (nc, storage) = open_fixture("netcdf-global/global_5deg.nc");
    let inspected = inspect(&nc, &storage, &Wanted::All, 0).unwrap();
    assert!(inspected.skipped.is_empty(), "{:?}", inspected.skipped);
    assert_eq!(inspected.run, None);
    assert_eq!(inspected.times.len(), 3);
    assert_eq!(
        inspected.times[2] - inspected.times[0],
        chrono::Duration::hours(12)
    );
    assert!(!inspected.grid.flipped);
    assert_eq!(inspected.grid.transform.crs, Crs::Wgs84);
    assert_eq!(
        inspected.grid.transform.bbox(),
        [-180.0, -90.0, 180.0, 90.0]
    );
    let read = |name: &str| {
        &inspected
            .fields
            .iter()
            .find(|f| &*f.read.name == name)
            .unwrap()
            .read
    };
    let packed = read("t_packed");
    assert_eq!(packed.stored, Stored::Short);
    assert_eq!(packed.packing.fill, [-32767.0]);
    assert_eq!(
        packed.packing.decode(-32767.0).to_bits(),
        f32::NAN.to_bits()
    );
    assert!((packed.packing.decode(1000.0) - 283.15).abs() < 1e-4);
    // Three steps of 72 x 36 floats, one chunk each.
    let float = read("air_temperature");
    assert_eq!(
        (float.field_bytes, float.chunk_bytes),
        (72 * 36 * 4, 72 * 36 * 4)
    );
}

#[test]
fn an_allowlisted_variable_missing_fails_the_file() {
    let (nc, storage) = open_fixture(MEPS_00);
    let names = vec!["air_temperature_2m".to_string(), "snowfall".to_string()];
    let error = inspect(&nc, &storage, &Wanted::Listed(&names), 0)
        .err()
        .unwrap();
    assert!(error.contains("'snowfall'"), "{error}");
}

fn dims(spec: &[(&str, u64, AxisRole)]) -> Vec<(String, u64, AxisRole)> {
    spec.iter()
        .map(|&(name, size, role)| (name.to_string(), size, role))
        .collect()
}

#[test]
fn single_level_fields_on_a_time_axis_are_served() {
    use AxisRole::*;
    let shape = classify(
        &dims(&[
            ("time", 1, Time),
            ("height6", 1, Other),
            ("y", 1069, ProjectionY),
            ("x", 949, ProjectionX),
        ]),
        0,
    )
    .unwrap();
    assert_eq!(
        shape.dims,
        [Dim::Time, Dim::Index(0), Dim::Whole(1069), Dim::Whole(949)]
    );
    assert_eq!(
        (shape.levels.as_slice(), shape.ny, shape.nx),
        (&[1][..], 1069, 949)
    );
    // An ensemble axis is read at the configured member; a size-1 run axis
    // is no level.
    let shape = classify(
        &dims(&[
            ("forecast_reference_time", 1, Reference),
            ("time", 3, Time),
            ("ensemble_member", 5, Ensemble),
            ("lat", 10, Lat),
            ("lon", 20, Lon),
        ]),
        2,
    )
    .unwrap();
    assert_eq!(
        shape.dims,
        [
            Dim::Index(0),
            Dim::Time,
            Dim::Index(2),
            Dim::Whole(10),
            Dim::Whole(20)
        ]
    );
    assert!(shape.levels.is_empty());
}

#[test]
fn fields_that_cannot_be_served_say_why() {
    use AxisRole::*;
    let reason =
        |spec: &[(&str, u64, AxisRole)], member| classify(&dims(spec), member).unwrap_err();
    // Pressure levels are an axis, never pinned to one silently.
    let error = reason(
        &[
            ("time", 1, Time),
            ("pressure", 13, Other),
            ("y", 4, ProjectionY),
            ("x", 4, ProjectionX),
        ],
        0,
    );
    assert!(
        error.contains("13 values on its 'pressure' axis"),
        "{error}"
    );
    assert!(reason(&[("y", 4, ProjectionY), ("x", 4, ProjectionX)], 0).contains("no time axis"));
    assert!(reason(
        &[
            ("time", 1, Time),
            ("x", 4, ProjectionX),
            ("y", 4, ProjectionY)
        ],
        0
    )
    .contains("not on a 1-D"));
    let error = reason(
        &[
            ("time", 1, Time),
            ("member", 3, Ensemble),
            ("lat", 4, Lat),
            ("lon", 4, Lon),
        ],
        3,
    );
    assert!(error.contains("member 3 is not one"), "{error}");
    assert!(reason(&[("lon", 4, Lon)], 0).contains("fewer than two"));
}

#[test]
fn fill_values_compare_in_the_stored_type() {
    assert_eq!(Stored::Float.normalize(9.96921e36), 9.96921e36_f32 as f64);
    assert_eq!(Stored::Short.normalize(-32767.0), -32767.0);
    let packing = Packing {
        scale: 0.5,
        offset: 10.0,
        fill: vec![Stored::Float.normalize(9.96921e36)],
    };
    assert!(packing.decode(9.96921e36_f32 as f64).is_nan());
    assert!(packing.decode(f64::NAN).is_nan());
    assert_eq!(packing.decode(4.0), 12.0);
}
