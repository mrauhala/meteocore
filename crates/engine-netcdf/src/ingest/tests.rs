use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use netcdf_reader::{NcAttribute, NcDimension, Storage, StorageBuffer};

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
        nc.read_variable_slice::<f32>(&read.name, &read.selection(0, 0))
            .unwrap();
        let (first, _) = storage.take();
        nc.read_variable_slice::<f32>(&read.name, &read.selection(0, 0))
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
    assert!(grid.flipped(), "MEPS's y ascends");
    let transform = grid.transform().expect("MEPS is on its Lambert axes");
    assert!(matches!(
        transform.crs,
        Crs::LambertConformalConic { radius: Some(r), .. } if r == 6_371_000.0
    ));
    assert_eq!((transform.width, transform.height), (10, 11));
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
    assert!(!inspected.grid.flipped());
    assert_eq!(inspected.grid.transform().unwrap().crs, Crs::Wgs84);
    assert_eq!(
        inspected.grid.transform().unwrap().bbox(),
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

/// No dimension is a pressure axis.
fn no_pressure(_: &str) -> bool {
    false
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
        false,
        no_pressure,
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
        false,
        no_pressure,
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
    let reason = |spec: &[(&str, u64, AxisRole)], member| {
        classify(&dims(spec), member, false, no_pressure).unwrap_err()
    };
    // An axis of several levels that is not a pressure axis, never pinned
    // to one level silently.
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

/// #1067 PR 6: a later file of the family spelling a unit another way
/// (`kg m-2` for the first file's `kg/m^2`) holds the same quantity and is
/// served; another unit, level or level unit is not.
#[test]
fn same_quantity_reads_units_in_any_spelling() {
    let var = |units: &str, level: f64, level_units: &str| Variable {
        name: "precipitation_amount_acc".into(),
        title: "Accumulated total precipitation".into(),
        standard_name: None,
        units: units.into(),
        levels: vec![Level {
            value: level,
            units: level_units.into(),
        }],
        pressure: None,
    };
    let served = var("kg/m^2", 0.0, "m");
    for same in [
        var("kg/m^2", 0.0, "m"),
        var("kg m-2", 0.0, "m"),
        var("kg m**-2", 0.0, " m "),
        var("kg.m-2", 0.0, "m"),
    ] {
        assert!(served.same_quantity(&same), "{same:?}");
    }
    for other in [
        var("m", 0.0, "m"),
        var("kg m-2 s-1", 0.0, "m"),
        var("kg/m^2", 2.0, "m"),
        var("kg/m^2", 0.0, "km"),
        Variable {
            levels: Vec::new(),
            ..var("kg/m^2", 0.0, "m")
        },
    ] {
        assert!(!served.same_quantity(&other), "{other:?}");
    }
}

// ---------------------------------------------------------------------------
// Pressure levels (#1067 PR 8)
// ---------------------------------------------------------------------------

const MEPS_PL_00: &str = "meps-netcdf-pl/meps_pl_00_20261015T06Z.nc";

/// MEPS's pressure-level fields sit on one `pressure` axis in hPa, stored
/// top first (300, 500, 850, 1000 in the fixture): served bottom first,
/// each served level read at its stored index, one level per read.
#[test]
fn meps_pressure_level_fields_are_on_the_pressure_axis_bottom_first() {
    let (nc, storage) = open_fixture(MEPS_PL_00);
    let inspected = inspect(&nc, &storage, &Wanted::All, 0).unwrap();
    let mut skipped: Vec<&str> = inspected.skipped.iter().map(|(n, _)| n.as_str()).collect();
    skipped.sort_unstable();
    assert_eq!(
        skipped,
        ["latitude", "longitude"],
        "{:?}",
        inspected.skipped
    );
    let mut names: Vec<&str> = inspected.fields.iter().map(|f| &*f.variable.name).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "air_temperature_pl",
            "geopotential_pl",
            "relative_humidity_pl",
            "x_wind_pl",
            "y_wind_pl"
        ]
    );
    for field in &inspected.fields {
        assert_eq!(
            field.variable.pressure.as_deref(),
            Some(&[1000.0, 850.0, 500.0, 300.0][..]),
            "{}",
            field.variable.name
        );
        assert!(field.variable.levels.is_empty(), "no single-level identity");
        let read = &field.read;
        assert_eq!(
            read.dims,
            [Dim::Time, Dim::Level, Dim::Whole(11), Dim::Whole(10)]
        );
        assert_eq!(read.levels, [3, 2, 1, 0]);
        // One level per chunk: a read decodes one field.
        assert_eq!((read.field_bytes, read.chunk_bytes), (440, 440));
        assert_eq!(read.decoded_bytes(), 440);
    }
    // Served level 1 (850 hPa) is stored at index 2.
    let read = &inspected
        .fields
        .iter()
        .find(|f| &*f.variable.name == "air_temperature_pl")
        .unwrap()
        .read;
    let selection = read.selection(0, 1);
    assert_eq!(
        selection.selections[..2],
        [NcSliceInfoElem::Index(0), NcSliceInfoElem::Index(2)]
    );
    let values = nc
        .read_variable_slice::<f32>(&read.name, &selection)
        .unwrap();
    let all = nc.read_variable_as_f64("air_temperature_pl").unwrap();
    let all: Vec<f64> = all.iter().copied().collect();
    let stored_850: Vec<f64> = all[2 * 110..3 * 110].to_vec();
    let read_850: Vec<f64> = values.iter().map(|&v| f64::from(v)).collect();
    assert_eq!(read_850.len(), 110);
    for (a, b) in read_850.iter().zip(&stored_850) {
        assert!((a - b).abs() < 1e-3, "{a} {b}");
    }
}

/// A pressure axis stored in Pa, top first, is served in hPa bottom first:
/// stored 50000, 85000, 100000 Pa (top first) are served as 1000, 850,
/// 500 hPa (bottom first), exactly.
#[test]
fn pressure_levels_in_pa_are_served_in_hpa() {
    let (nc, storage) = open_fixture("netcdf-pressure-pa/pressure_levels_pa.nc");
    let inspected = inspect(&nc, &storage, &Wanted::All, 0).unwrap();
    assert!(inspected.skipped.is_empty(), "{:?}", inspected.skipped);
    assert_eq!(inspected.fields.len(), 3);
    for field in &inspected.fields {
        assert_eq!(
            field.variable.pressure.as_deref(),
            Some(&[1000.0, 850.0, 500.0][..])
        );
        assert_eq!(field.read.levels, [2, 1, 0]);
    }
    assert_eq!(inspected.grid.transform().unwrap().crs, Crs::Wgs84);
    assert_eq!(inspected.run, None);
}

/// Classification: a dimension the coordinate's units make a pressure axis
/// is the field's vertical axis, at most one; a size-1 pressure axis stays
/// a single level's identity; any other axis of several values (height
/// above ground, hybrid levels) is not served.
#[test]
fn a_pressure_axis_is_the_one_vertical_axis() {
    use AxisRole::*;
    let pressure = |name: &str| name.starts_with("pressure");
    let shape = classify(
        &dims(&[
            ("time", 1, Time),
            ("pressure", 13, Other),
            ("y", 1069, ProjectionY),
            ("x", 949, ProjectionX),
        ]),
        0,
        false,
        pressure,
    )
    .unwrap();
    assert_eq!(
        shape.dims,
        [Dim::Time, Dim::Level, Dim::Whole(1069), Dim::Whole(949)]
    );
    assert_eq!(shape.vertical, Some(1));
    assert!(shape.levels.is_empty());
    // One pressure level is a level's identity, as before.
    let shape = classify(
        &dims(&[
            ("time", 1, Time),
            ("pressure0", 1, Other),
            ("y", 4, ProjectionY),
            ("x", 4, ProjectionX),
        ]),
        0,
        false,
        pressure,
    )
    .unwrap();
    assert_eq!(shape.vertical, None);
    assert_eq!(shape.levels, [1]);
    // A second pressure axis, or another axis of several levels.
    for (spec, error) in [
        (
            [
                ("time", 1, Time),
                ("pressure", 13, Other),
                ("pressure1", 4, Other),
                ("y", 4, ProjectionY),
                ("x", 4, ProjectionX),
            ],
            "4 values on its 'pressure1' axis",
        ),
        (
            [
                ("time", 1, Time),
                ("height_above_ground", 3, Other),
                ("pressure", 13, Other),
                ("y", 4, ProjectionY),
                ("x", 4, ProjectionX),
            ],
            "3 values on its 'height_above_ground' axis",
        ),
    ] {
        let reason = classify(&dims(&spec), 0, false, pressure).unwrap_err();
        assert!(reason.contains(error), "{reason}");
    }
}

/// A `float` variable `name(dims)` with text attributes, as the metadata
/// lists it.
fn variable(name: &str, dims: &[(&str, u64)], attrs: &[(&str, &str)]) -> NcVariable {
    NcVariable {
        name: name.to_string(),
        dimensions: dims
            .iter()
            .map(|&(name, size)| NcDimension {
                name: name.to_string(),
                size,
                is_unlimited: false,
            })
            .collect(),
        dtype: NcType::Float,
        attributes: attrs
            .iter()
            .map(|&(name, value)| NcAttribute {
                name: name.to_string(),
                value: NcAttrValue::Chars(value.to_string()),
            })
            .collect(),
        data_offset: 0,
        _data_size: 0,
        is_record_var: false,
        record_size: 0,
    }
}

/// A CF parametric (hybrid) vertical coordinate is not a pressure axis,
/// whatever its units (CF 4.3.3): CESM/CAM's `lev` has `units = "hPa"`,
/// a hybrid `standard_name` and `formula_terms`, and a field on it is not
/// served, for the reason any other axis of several levels gives. An
/// `air_pressure` coordinate, or one without a `standard_name` (old ERA5
/// files' `level` in millibars), is one.
#[test]
fn a_hybrid_coordinate_in_pressure_units_is_not_a_pressure_axis() {
    let hybrid = [
        (
            "standard_name",
            "atmosphere_hybrid_sigma_pressure_coordinate",
        ),
        ("units", "hPa"),
        ("positive", "down"),
        ("formula_terms", "a: hyam b: hybm p0: P0 ps: PS"),
    ];
    let axis = |attrs: &[(&str, &str)]| {
        let variables = [variable("lev", &[("lev", 32)], attrs)];
        pressure_unit(&variables, "lev")
    };
    assert_eq!(axis(&hybrid), None, "CESM/CAM");
    assert_eq!(axis(&hybrid[..3]), None, "a hybrid standard_name alone");
    assert_eq!(
        axis(&[("units", "hPa"), ("formula_terms", "a: hyam b: hybm")]),
        None,
        "formula_terms alone"
    );
    assert_eq!(
        axis(&[("standard_name", "air_pressure"), ("units", "Pa")]),
        Some(PressureUnit::Pa)
    );
    assert_eq!(
        axis(&[("long_name", "pressure_level"), ("units", "millibars")]),
        Some(PressureUnit::HPa)
    );
    assert_eq!(axis(&[("standard_name", "air_pressure")]), None, "no units");

    // A CAM temperature on that `lev` is not served.
    let variables = [
        variable(
            "time",
            &[("time", 1)],
            &[("units", "days since 2000-01-01 00:00:00")],
        ),
        variable("lev", &[("lev", 32)], &hybrid),
        variable("lat", &[("lat", 4)], &[("units", "degrees_north")]),
        variable("lon", &[("lon", 5)], &[("units", "degrees_east")]),
        variable(
            "T",
            &[("time", 1), ("lev", 32), ("lat", 4), ("lon", 5)],
            &[("units", "K")],
        ),
    ];
    let (nc, _) = open_fixture(MEPS_PL_00);
    let reason = field(&nc, None, &variables, &variables[4], 0)
        .err()
        .unwrap();
    assert_eq!(
        reason,
        "has 32 values on its 'lev' axis; only fields with one level, or with levels on one \
         pressure axis, are served"
    );
}

/// A served field of `name` with `pressure` levels, for the vertical-axis
/// rule.
fn field_on(name: &str, pressure: Option<&[f64]>) -> Field {
    Field {
        variable: Variable {
            name: name.into(),
            title: name.into(),
            units: "K".into(),
            standard_name: None,
            levels: Vec::new(),
            pressure: pressure.map(<[f64]>::to_vec),
        },
        read: FieldRead {
            name: name.into(),
            dims: vec![Dim::Time, Dim::Whole(2), Dim::Whole(2)],
            stored: Stored::Float,
            packing: Packing {
                scale: 1.0,
                offset: 0.0,
                fill: Vec::new(),
            },
            chunk_bytes: 16,
            field_bytes: 16,
            levels: Vec::new(),
        },
    }
}

/// One vertical axis per collection: with an allowlist the first listed
/// variable decides and a variable on another axis fails the file; without
/// one the largest group of fields on the same levels wins (one level on a
/// tie) and the others are skipped with why.
#[test]
fn a_collection_serves_one_vertical_axis() {
    let levels = [1000.0, 850.0, 500.0];
    let names = |fields: &[Field]| -> Vec<String> {
        fields.iter().map(|f| f.variable.name.to_string()).collect()
    };
    // Without an allowlist: two on levels outvote one single-level field.
    let mut skipped = Vec::new();
    let kept = one_vertical_axis(
        vec![
            field_on("surface_air_pressure", None),
            field_on("t_pl", Some(&levels)),
            field_on("u_pl", Some(&levels)),
            field_on("w_pl", Some(&[700.0])),
        ],
        &Wanted::All,
        &mut skipped,
    )
    .unwrap();
    assert_eq!(names(&kept), ["t_pl", "u_pl"]);
    let reasons: Vec<&str> = skipped.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(reasons, ["surface_air_pressure", "w_pl"]);
    assert!(skipped[0].1.contains("one vertical axis"), "{skipped:?}");
    assert!(skipped[1].1.contains("700 hPa"), "{skipped:?}");
    // A tie keeps the single-level fields, as an sfc file is served.
    let mut skipped = Vec::new();
    let kept = one_vertical_axis(
        vec![field_on("t_pl", Some(&levels)), field_on("t2m", None)],
        &Wanted::All,
        &mut skipped,
    )
    .unwrap();
    assert_eq!(names(&kept), ["t2m"]);
    assert!(skipped[0].1.contains("3 pressure levels"), "{skipped:?}");
    // Groups are counted by their levels: three single-level fields beat
    // two pressure groups of two, though four fields are on pressure
    // levels.
    let upper = [500.0, 300.0];
    let mut skipped = Vec::new();
    let kept = one_vertical_axis(
        vec![
            field_on("t_lower", Some(&levels[..2])),
            field_on("t2m", None),
            field_on("u_lower", Some(&levels[..2])),
            field_on("t_upper", Some(&upper)),
            field_on("u_upper", Some(&upper)),
            field_on("rh2m", None),
            field_on("msl", None),
        ],
        &Wanted::All,
        &mut skipped,
    )
    .unwrap();
    assert_eq!(names(&kept), ["t2m", "rh2m", "msl"]);
    assert_eq!(skipped.len(), 4, "{skipped:?}");
    // The largest pressure group wins, not the first field's levels; of
    // groups as large, the first found.
    let mut skipped = Vec::new();
    let kept = one_vertical_axis(
        vec![
            field_on("t_lower", Some(&levels[..2])),
            field_on("t_upper", Some(&upper)),
            field_on("u_upper", Some(&upper)),
            field_on("t2m", None),
        ],
        &Wanted::All,
        &mut skipped,
    )
    .unwrap();
    assert_eq!(names(&kept), ["t_upper", "u_upper"]);
    assert!(skipped[0].1.contains("500, 300 hPa"), "{skipped:?}");
    let kept = one_vertical_axis(
        vec![
            field_on("t_lower", Some(&levels[..2])),
            field_on("t_upper", Some(&upper)),
        ],
        &Wanted::All,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(names(&kept), ["t_lower"]);
    // An allowlist: the first listed decides, and a mix fails the file.
    let listed = vec!["t_pl".to_string(), "t2m".to_string()];
    let error = one_vertical_axis(
        vec![field_on("t_pl", Some(&levels)), field_on("t2m", None)],
        &Wanted::Listed(&listed),
        &mut Vec::new(),
    )
    .err()
    .unwrap();
    assert!(error.contains("variable 't2m' has one level"), "{error}");
}

/// A later file of a pressure family must have the same levels, in hPa,
/// whatever its own unit or order.
#[test]
fn same_quantity_compares_pressure_levels() {
    let on = |pressure: Option<&[f64]>| field_on("t_pl", pressure).variable;
    let served = on(Some(&[1000.0, 850.0]));
    assert!(served.same_quantity(&on(Some(&[1000.0, 850.0]))));
    assert!(!served.same_quantity(&on(Some(&[1000.0, 850.0, 500.0]))));
    assert!(!served.same_quantity(&on(Some(&[925.0, 850.0]))));
    assert!(!served.same_quantity(&on(None)));
}

const CURVILINEAR: &str = "netcdf-curvilinear/svalbard_curvilinear.nc";

/// #1174: fields whose CF `coordinates` name 2-D latitude/longitude, with
/// no 1-D coordinates on their `(y, x)`, are on a curvilinear grid: both
/// arrays read once per file, rows as stored, compared node by node with
/// another file's. The auxiliaries themselves have no time axis, and are
/// skipped as MEPS's are.
#[test]
fn two_dimensional_latitude_longitude_make_a_curvilinear_grid() {
    let (nc, storage) = open_fixture(CURVILINEAR);
    let inspected = inspect(&nc, &storage, &Wanted::All, 0).unwrap();
    let names: Vec<&str> = inspected.fields.iter().map(|f| &*f.variable.name).collect();
    assert_eq!(names, ["column", "row"]);
    let mut skipped: Vec<&str> = inspected.skipped.iter().map(|(n, _)| n.as_str()).collect();
    skipped.sort_unstable();
    assert_eq!(skipped, ["lat", "lon"], "{:?}", inspected.skipped);
    let FileGrid::Curvilinear(nodes) = &inspected.grid else {
        panic!("not curvilinear: {:?}", inspected.grid);
    };
    assert_eq!(nodes.size(), (19, 28));
    assert_eq!(inspected.grid.size(), (19, 28));
    assert!(!inspected.grid.flipped() && inspected.grid.transform().is_none());
    assert_eq!(
        inspected.fields[0].read.dims,
        [Dim::Time, Dim::Whole(28), Dim::Whole(19)]
    );
    let names = vec!["row".to_string()];
    let again = inspect(&nc, &storage, &Wanted::Listed(&names), 0).unwrap();
    assert!(inspected.grid.same(&again.grid));
    let (meps, meps_storage) = open_fixture(MEPS_00);
    let meps = inspect(&meps, &meps_storage, &Wanted::All, 0).unwrap();
    assert!(!inspected.grid.same(&meps.grid) && !meps.grid.same(&inspected.grid));
}

/// How a field's `(y, x)` are placed: 2-D auxiliaries by their
/// `standard_name` or `units`, never their names; with both, 1-D axes win
/// only as their own attributes state them (latitude/longitude, or
/// projected x/y with a `grid_mapping`, as MEPS), never as a bare index
/// axis's name reads.
#[test]
fn stated_axes_win_over_auxiliaries_and_names_decide_nothing() {
    let (ny, nx) = (3, 4);
    let grid_var = |name: &str, (y, x): (&str, &str), attrs: &[(&str, &str)]| {
        variable(name, &[(y, ny), (x, nx)], attrs)
    };
    let place = |(y, x): (&str, &str), axes: Vec<NcVariable>, attrs: &[(&str, &str)]| {
        let mut variables = vec![
            grid_var("nav_lat", (y, x), &[("units", "degrees_north")]),
            grid_var("nav_lon", (y, x), &[("standard_name", "longitude")]),
            grid_var("latitude", (y, x), &[]),
            grid_var("longitude", (y, x), &[]),
            grid_var("lat2", (y, x), &[("standard_name", "latitude")]),
            grid_var("lat_t", (x, y), &[("standard_name", "latitude")]),
        ];
        variables.extend(axes);
        let field = variable("t", &[("time", 1), (y, ny), (x, nx)], attrs);
        placement(&variables, &field)
    };
    let aux = || {
        Some(Placement::Auxiliary {
            lat: "nav_lat".into(),
            lon: "nav_lon".into(),
        })
    };
    let coords = ("coordinates", "nav_lon nav_lat");
    let yx = ("y", "x");
    assert_eq!(place(("nj", "ni"), vec![], &[coords]), aux());
    // Index axes named y and x read as latitude/longitude by their names:
    // the auxiliaries win; without them, the names do, as before.
    let index = || {
        vec![
            variable("y", &[("y", ny)], &[]),
            variable("x", &[("x", nx)], &[]),
        ]
    };
    assert_eq!(place(yx, index(), &[coords]), aux());
    assert_eq!(place(yx, index(), &[]), Some(Placement::Axes(None)));
    // Projected axes stated, with a grid mapping (MEPS): the axes.
    let projected = || {
        vec![
            variable(
                "y",
                &[("y", ny)],
                &[("standard_name", "projection_y_coordinate"), ("units", "m")],
            ),
            variable(
                "x",
                &[("x", nx)],
                &[("standard_name", "projection_x_coordinate"), ("units", "m")],
            ),
        ]
    };
    assert_eq!(
        place(yx, projected(), &[coords, ("grid_mapping", "crs")]),
        Some(Placement::Axes(Some("crs".into())))
    );
    assert_eq!(place(yx, projected(), &[coords]), aux());
    // Latitude/longitude axes stated: the axes.
    let degrees = vec![
        variable("y", &[("y", ny)], &[("units", "degrees_north")]),
        variable("x", &[("x", nx)], &[("units", "degrees_east")]),
    ];
    assert_eq!(place(yx, degrees, &[coords]), Some(Placement::Axes(None)));
    // Auxiliaries known only by name, two latitudes, or one on (x, y):
    // nothing places dimensions without a role.
    let nowhere = ("nj", "ni");
    for named in [
        "longitude latitude",
        "nav_lon nav_lat lat2",
        "nav_lon lat_t",
        "nav_lon",
    ] {
        assert_eq!(
            place(nowhere, vec![], &[("coordinates", named)]),
            None,
            "{named}"
        );
    }
}
