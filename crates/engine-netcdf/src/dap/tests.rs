//! The DAP2 formats on what thredds.met.no served for MEPS on 2026-10-23
//! (testdata/netcdf-opendap: a pl file's whole DDS and DAS, an sfc file's
//! trimmed to the example's variables, the coordinate replies a file's
//! open asks for, and a 4 × 4 hyperslab of 2 m temperature), and on
//! synthetic documents for what MEPS does not exercise: every XDR type,
//! escapes, signed bytes and errors.

use chrono::{DateTime, Utc};
use netcdf_reader::NcSliceInfoElem;

use super::*;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../testdata/netcdf-opendap/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn text(name: &str) -> String {
    String::from_utf8(fixture(name)).unwrap()
}

fn variable<'a>(metadata: &'a Metadata, name: &str) -> &'a NcVariable {
    metadata.variables.iter().find(|v| v.name == name).unwrap()
}

fn attribute<'a>(var: &'a NcVariable, name: &str) -> &'a NcAttrValue {
    &var.attribute(name).unwrap().value
}

/// netCDF's default `float` fill.
const NC_FILL_FLOAT: f32 = 9.969_21e36;

/// THREDDS's MEPS pl metadata as netcdf-reader would describe the file:
/// fields as Grids projected by their array, coordinates and scalars by
/// name, CF attributes typed as stored, the record dimension unlimited,
/// and the fill printed `9.96921E36` exactly netCDF's default fill.
#[test]
fn thredds_meps_metadata_reads_as_netcdf_variables() {
    let pl = metadata(
        &text("meps_pl_00_20261023T12Z.nc.dds"),
        &text("meps_pl_00_20261023T12Z.nc.das"),
    )
    .unwrap();
    let names: Vec<&str> = pl.variables.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(
        names.len(),
        23,
        "6 coordinates and scalars, 17 Grids: {names:?}"
    );
    let t = variable(&pl, "air_temperature_pl");
    let dims: Vec<(&str, u64)> = t
        .dimensions
        .iter()
        .map(|d| (d.name.as_str(), d.size))
        .collect();
    assert_eq!(
        dims,
        [("time", 1), ("pressure", 13), ("y", 1069), ("x", 949)]
    );
    assert!(t.dimensions[0].is_unlimited && !t.dimensions[1].is_unlimited);
    assert_eq!(t.dtype, NcType::Float);
    assert_eq!(
        pl.projection("air_temperature_pl"),
        Some("air_temperature_pl.air_temperature_pl")
    );
    assert_eq!(pl.projection("x"), Some("x"));
    assert_eq!(attribute(t, "units").as_string().as_deref(), Some("K"));
    assert_eq!(
        attribute(t, "grid_mapping").as_string().as_deref(),
        Some("projection_lambert")
    );
    assert_eq!(
        attribute(t, "_FillValue"),
        &NcAttrValue::Floats(vec![NC_FILL_FLOAT])
    );
    assert_eq!(NC_FILL_FLOAT as f64, 9.969_209_968_386_869e36);

    let run = variable(&pl, "forecast_reference_time");
    assert!(run.dimensions.is_empty() && run.dtype == NcType::Double);
    let lambert = variable(&pl, "projection_lambert");
    assert_eq!(lambert.dtype, NcType::Int);
    assert_eq!(
        attribute(lambert, "standard_parallel"),
        &NcAttrValue::Doubles(vec![63.3, 63.3])
    );
    assert_eq!(
        attribute(lambert, "earth_radius"),
        &NcAttrValue::Doubles(vec![6_371_000.0])
    );
    let pressure = variable(&pl, "pressure");
    assert!(pressure.is_coordinate_variable());
    assert_eq!(
        attribute(pressure, "units").as_string().as_deref(),
        Some("hPa")
    );
    assert_eq!(
        attribute(variable(&pl, "latitude"), "_ChunkSizes"),
        &NcAttrValue::UInts(vec![1069, 949])
    );

    // The sfc file, trimmed: levels by value on their own axes, and the
    // global attributes in UTF-8.
    let sfc_das = parse_das(&text("meps_sfc_00_20261023T12Z.nc.das")).unwrap();
    let creator = sfc_das.child("NC_GLOBAL").unwrap().text("creator_name");
    assert_eq!(
        creator.as_deref(),
        Some("Senter for utvikling av værvarslingstjenesten")
    );
    let sfc = metadata(
        &text("meps_sfc_00_20261023T12Z.nc.dds"),
        &text("meps_sfc_00_20261023T12Z.nc.das"),
    )
    .unwrap();
    let t2m = variable(&sfc, "air_temperature_2m");
    assert_eq!(t2m.dimensions[1].name, "height1");
    assert_eq!(
        attribute(variable(&sfc, "precipitation_amount_acc"), "units")
            .as_string()
            .as_deref(),
        Some("kg/m^2")
    );
}

/// The replies THREDDS sent: scalars bare, arrays after their length twice,
/// a field projected from its Grid inside a Structure, in dataset order
/// whatever the constraint's.
#[test]
fn thredds_replies_decode() {
    let values = decode(&fixture("meps_sfc_00_20261023T12Z_t2m_4x4.dods")).unwrap();
    let names: Vec<&str> = values.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "time",
            "forecast_reference_time",
            "height1",
            "projection_lambert",
            "x",
            "air_temperature_2m"
        ]
    );
    let run: DateTime<Utc> = "2026-10-23T12:00:00Z".parse().unwrap();
    assert_eq!(values[0].1, Raw::F64(vec![run.timestamp() as f64]));
    assert_eq!(values[1].1, Raw::F64(vec![run.timestamp() as f64]));
    assert_eq!(values[2].1, Raw::F32(vec![2.0]));
    assert_eq!(
        values[3].1,
        Raw::I32(vec![-2_147_483_647]),
        "netCDF's int fill"
    );
    let Raw::F32(x) = &values[4].1 else {
        panic!("{:?}", values[4]);
    };
    assert_eq!(x.len(), 4);
    assert!(
        x.windows(2).all(|pair| pair[1] - pair[0] == 2500.0),
        "{x:?}"
    );
    let t2m = take(values, "air_temperature_2m").unwrap();
    let Raw::F32(kelvin) = &t2m else {
        panic!("{t2m:?}");
    };
    assert_eq!(kelvin.len(), 16);
    assert_eq!(kelvin[0], 278.412_93);
    assert!(
        kelvin.iter().all(|k| (270.0..290.0).contains(k)),
        "{kelvin:?}"
    );

    let coordinates = decode(&fixture("meps_pl_00_20261023T12Z_coordinates.dods")).unwrap();
    let pressure = take(coordinates.clone(), "pressure").unwrap().into_f64();
    assert_eq!(
        pressure,
        [
            50.0, 100.0, 150.0, 200.0, 250.0, 300.0, 400.0, 500.0, 700.0, 800.0, 850.0, 925.0,
            1000.0
        ]
    );
    assert_eq!(take(coordinates.clone(), "x").unwrap().len(), 949);
    assert_eq!(take(coordinates, "y").unwrap().len(), 1069);
}

/// A reply of `dds` declarations and `data` bytes.
fn reply(dds: &str, data: &[u8]) -> Vec<u8> {
    let mut reply = format!("Dataset {{\n{dds}}} test.nc;\n\nData:\n").into_bytes();
    reply.extend_from_slice(data);
    reply
}

/// XDR words.
fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|w| w.to_be_bytes()).collect()
}

/// Every base type the engine reads, as XDR sends it: `Int16` and `UInt16`
/// widened to 32 bits, a `Byte` array padded to four bytes and a scalar
/// `Byte` in a word, `Float64` in eight bytes; structures and Grids
/// member by member. A short reply, counts that disagree with the shape,
/// and string values are errors.
#[test]
fn every_type_the_engine_reads_decodes() {
    let dds = "Byte b[n = 5];\nByte one;\nInt16 s[n = 3];\nUInt16 us[n = 2];\n\
               Int32 i[n = 2];\nUInt32 u;\nFloat32 f[n = 2];\nFloat64 d[n = 2];\n\
               Structure {\n Int16 member[m = 1];\n} st;\n\
               Grid {\n ARRAY:\n Float32 g[y = 1][x = 2];\n MAPS:\n Float64 y[y = 1];\n \
               Float64 x[x = 2];\n} g;\n";
    let mut data = words(&[5, 5]);
    data.extend_from_slice(&[1, 2, 250, 4, 5, 0, 0, 0]);
    data.extend(words(&[200]));
    data.extend(words(&[3, 3, (-2i32) as u32, 7, (-32768i32) as u32]));
    data.extend(words(&[2, 2, 65535, 1]));
    data.extend(words(&[2, 2, (-5i32) as u32, 2_000_000_000]));
    data.extend(words(&[4_000_000_000]));
    data.extend(words(&[2, 2, 1.5f32.to_bits(), f32::NAN.to_bits()]));
    data.extend(words(&[2, 2]));
    data.extend(0.25f64.to_be_bytes());
    data.extend((-1e300f64).to_be_bytes());
    data.extend(words(&[1, 1, 9]));
    data.extend(words(&[2, 2, 7.0f32.to_bits(), 8.0f32.to_bits(), 1, 1]));
    data.extend(60.0f64.to_be_bytes());
    data.extend(words(&[2, 2]));
    data.extend(10.0f64.to_be_bytes());
    data.extend(11.0f64.to_be_bytes());
    let values = decode(&reply(dds, &data)).unwrap();
    let raw = |name: &str| take(values.clone(), name).unwrap();
    assert_eq!(raw("b"), Raw::U8(vec![1, 2, 250, 4, 5]));
    assert_eq!(raw("one"), Raw::U8(vec![200]));
    assert_eq!(raw("s"), Raw::I16(vec![-2, 7, -32768]));
    assert_eq!(raw("us"), Raw::U16(vec![65535, 1]));
    assert_eq!(raw("i"), Raw::I32(vec![-5, 2_000_000_000]));
    assert_eq!(raw("u"), Raw::U32(vec![4_000_000_000]));
    let Raw::F32(f) = raw("f") else { panic!() };
    assert!(f[0] == 1.5 && f[1].is_nan());
    assert_eq!(raw("d"), Raw::F64(vec![0.25, -1e300]));
    assert_eq!(raw("member"), Raw::I16(vec![9]));
    assert_eq!(raw("g"), Raw::F32(vec![7.0, 8.0]));
    assert_eq!(raw("y"), Raw::F64(vec![60.0]));
    assert_eq!(raw("x"), Raw::F64(vec![10.0, 11.0]));

    let short = reply(dds, &data[..data.len() - 4]);
    assert!(decode(&short).unwrap_err().contains("short"));
    let counts = reply("Float32 f[n = 3];\n", &words(&[2, 2, 0, 0]));
    assert!(decode(&counts).unwrap_err().contains("shape of 3"));
    let strings = reply("String s;\n", &words(&[1, 0x41000000]));
    assert!(decode(&strings).unwrap_err().contains("string"));
}

/// What an attribute's text says, in its type: escapes inside quotes, a
/// list of values, `NaN` and Java's `Infinity`, negative bytes; an
/// `Alias` is not read; a value that is no number is an error, never
/// dropped. DAP2's `Byte` is unsigned: a variable `_Unsigned = "false"`
/// is a signed byte, its attributes in its own type.
#[test]
fn das_attributes_take_their_types_and_escapes() {
    let das = r#"Attributes {
    v {
        String note "a \"quoted\" \\ word", "second";
        String units "K";
        Float32 nan NaN, Infinity, -Infinity;
        Int16 shorts -3, 4;
        UInt16 ushorts 65535;
        Byte bytes -127, 3;
        Alias other v.units;
        inner {
            Int32 deep 7;
        }
    }
    b {
        Byte _FillValue 129;
        String _Unsigned "false";
    }
    u {
        Byte _FillValue 255;
    }
}"#;
    let parsed = parse_das(das).unwrap();
    let v = parsed.child("v").unwrap();
    let value = |name: &str| {
        v.attributes
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.value.clone())
    };
    assert_eq!(
        value("note"),
        Some(NcAttrValue::Strings(vec![
            r#"a "quoted" \ word"#.into(),
            "second".into()
        ]))
    );
    assert_eq!(value("units"), Some(NcAttrValue::Chars("K".into())));
    let Some(NcAttrValue::Floats(special)) = value("nan") else {
        panic!()
    };
    assert!(special[0].is_nan() && special[1] == f32::INFINITY && special[2] == -f32::INFINITY);
    assert_eq!(value("shorts"), Some(NcAttrValue::Shorts(vec![-3, 4])));
    assert_eq!(value("ushorts"), Some(NcAttrValue::UShorts(vec![65535])));
    assert_eq!(value("bytes"), Some(NcAttrValue::Bytes(vec![-127, 3])));
    assert_eq!(value("other"), None);
    assert_eq!(
        v.child("inner").unwrap().attributes[0].value,
        NcAttrValue::Ints(vec![7])
    );

    let dds = "Dataset {\n    Byte b[n = 2];\n    Byte u[n = 2];\n} x;";
    let meta = metadata(dds, das).unwrap();
    let (b, u) = (variable(&meta, "b"), variable(&meta, "u"));
    assert_eq!(
        (b.dtype.clone(), u.dtype.clone()),
        (NcType::Byte, NcType::UByte)
    );
    assert_eq!(attribute(b, "_FillValue"), &NcAttrValue::Bytes(vec![-127]));
    assert_eq!(attribute(u, "_FillValue"), &NcAttrValue::UBytes(vec![255]));

    let bad = "Attributes {\n v {\n Float32 _FillValue 1e;\n }\n}";
    assert!(parse_das(bad).unwrap_err().contains("_FillValue"));
    assert!(parse_das("<html>").is_err());
    assert!(parse_dds("Dataset { Int64 x; } d;")
        .unwrap_err()
        .contains("Int64"));
}

/// A field's hyperslab: an index per fixed axis, a range per grid axis,
/// the stop inclusive, brackets percent-encoded for the query.
#[test]
fn constraints_select_the_exact_hyperslab() {
    let selection = NcSliceInfo {
        selections: vec![
            NcSliceInfoElem::Index(0),
            NcSliceInfoElem::Index(12),
            NcSliceInfoElem::Slice {
                start: 0,
                end: 1069,
                step: 1,
            },
            NcSliceInfoElem::Slice {
                start: 0,
                end: 949,
                step: 1,
            },
        ],
    };
    assert_eq!(
        constraint("t.t", &selection),
        "t.t%5B0%5D%5B12%5D%5B0:1:1068%5D%5B0:1:948%5D"
    );
    assert_eq!(whole(&["x", "a b", "g.g"]), "x,a%20b,g.g");
    assert_eq!(xdr_bytes(&NcType::Float, 4), 24);
    assert_eq!(xdr_bytes(&NcType::Short, 4), 24, "widened to 32 bits");
    assert_eq!(xdr_bytes(&NcType::UByte, 5), 16, "padded");
    assert_eq!(xdr_bytes(&NcType::Double, 2), 24);
}

/// A reply without data is the server's error, by its message.
#[test]
fn a_reply_without_data_is_the_servers_error() {
    let error = b"Error {\n    code = 400;\n    message = \"Unrecognized request\";\n};\n";
    let message = decode(error).unwrap_err();
    assert!(message.contains("Unrecognized request"), "{message}");
    assert!(check(error).is_err());
    assert!(check(&fixture("meps_sfc_00_20261023T12Z_t2m_4x4.dods")).is_ok());
    assert_eq!(decode(b"<html/>").unwrap_err(), "the reply carries no data");
}
