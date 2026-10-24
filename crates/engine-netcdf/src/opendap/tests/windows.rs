//! EDR queries through OPeNDAP read windows of their fields (#1171): one
//! hyperslab per field, covering the cells its positions sample, never the
//! whole field unless the window would pass half of it, and the answer is
//! the one the whole field gives, value for value. The levels a query asks
//! of one field at one step are one hyperslab over the stored levels
//! between them, while that stays cheap: one request per variable and
//! step, which the read cap counts once. Every query here is asked of the
//! same files read locally, which read whole fields.

use std::time::{Duration, Instant};

use ds_core::cube::CubeResolution;
use ds_core::deadline;
use ds_core::feature::Bbox;
use ds_core::model::CoverageResponse;

use super::*;
use crate::edr::Limits;

/// One coverage of an answer: its domain, and its ranges by parameter
/// (shape and values).
type Coverage = (String, BTreeMap<String, (Vec<usize>, Vec<Option<f64>>)>);

/// An EDR answer, ordered so two engines' compare; an error as its text.
type Answer = Result<Vec<Coverage>, String>;

fn answer(response: Result<CoverageResponse, DataServerError>) -> Answer {
    let results = match response.map_err(|e| format!("{e:?}"))? {
        CoverageResponse::Single(result) => vec![result],
        CoverageResponse::Collection(results) => results,
    };
    Ok(results
        .into_iter()
        .map(|result| {
            let ranges = result
                .ranges
                .into_iter()
                .map(|(name, range)| (name, (range.shape, range.values)))
                .collect();
            (format!("{:?}", result.domain), ranges)
        })
        .collect())
}

/// A multi-point query's answers, one per point.
fn answers(engine: &NetcdfEngine, points: &[String], parameters: &[String]) -> Vec<Answer> {
    let mut answered = Vec::new();
    engine
        .query_positions(points, None, Some(parameters), None, None, &mut |r| {
            answered.push(answer(Ok(r)));
            Ok(())
        })
        .unwrap();
    answered
}

/// `(columns, rows)` of the engine's grid.
fn grid(engine: &NetcdfEngine) -> (u32, u32) {
    engine
        .catalog
        .load_full()
        .layout
        .as_ref()
        .unwrap()
        .source
        .size()
}

/// WGS84 points at fractional cell positions `(column, row)` of the
/// engine's grid, measured from the first cell's centre, row 0 north.
fn at_cells(engine: &NetcdfEngine, cells: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let catalog = engine.catalog.load_full();
    let gt = catalog
        .layout
        .as_ref()
        .unwrap()
        .grid
        .transform()
        .unwrap()
        .clone();
    cells
        .iter()
        .map(|&(col, row)| {
            let x = gt.origin_x + (col + 0.5) * gt.pixel_width;
            let y = gt.origin_y - (row + 0.5) * gt.pixel_height;
            gt.crs.inverse(x, y).unwrap()
        })
        .collect()
}

fn point((lon, lat): (f64, f64)) -> String {
    format!("POINT({lon} {lat})")
}

/// A polygon over the box of `corners`.
fn polygon(corners: &[(f64, f64)]) -> String {
    let lons = corners.iter().map(|c| c.0);
    let lats = corners.iter().map(|c| c.1);
    let (w, e) = (
        lons.clone().fold(f64::MAX, f64::min),
        lons.fold(f64::MIN, f64::max),
    );
    let (s, n) = (
        lats.clone().fold(f64::MAX, f64::min),
        lats.fold(f64::MIN, f64::max),
    );
    format!("POLYGON(({w} {s}, {e} {s}, {e} {n}, {w} {n}, {w} {s}))")
}

/// The `.dods` reads of `variable` among `requests`: each one's file name
/// and its hyperslabs, `[i]` or `[start:stride:stop]` per axis.
fn reads_of(requests: &[Request], variable: &str) -> Vec<(String, Vec<String>)> {
    let grid = format!("{variable}.{variable}");
    requests
        .iter()
        .filter_map(|r| {
            let (file, query) = r.path.split_once(".dods?")?;
            let query = query.replace("%5B", "[").replace("%5D", "]");
            let slabs = query
                .strip_prefix(&grid)
                .or_else(|| query.strip_prefix(variable))
                .filter(|slabs| slabs.starts_with('['))?;
            let file = file.rsplit('/').next()?.to_string();
            Some((file, slabs.split_inclusive(']').map(String::from).collect()))
        })
        .collect()
}

/// The cells a hyperslab `[start:1:stop]` spans.
fn span(slab: &str) -> u64 {
    let numbers: Vec<u64> = slab
        .trim_matches(|c| c == '[' || c == ']')
        .split(':')
        .map(|n| n.parse().unwrap())
        .collect();
    numbers[numbers.len() - 1] - numbers[0] + 1
}

/// A position series at a point reads one window per lead: the 2 × 2 cells
/// around the point, rows counted north first and read where MEPS stores
/// them, south-up. It answers what the files read whole answer, and asked
/// again, every window comes from the chunk cache.
#[test]
fn a_position_series_reads_a_window_of_each_field() {
    let _pool = crate::remote::tests::query_pool_shared();
    let files = sfc_files();
    let (_dir, local) = local(&files, "sfc", SFC_VARIABLES);
    let server = TestServer::new();
    publish(&server, &files, true, None);
    let dap = opened(&config(&server, "sfc", &[0, 1, 2], SFC_VARIABLES));
    assert_eq!(grid(&dap), (10, 11));
    server.take();
    let here = point(at_cells(&dap, &[(4.3, 5.6)])[0]);
    let t2m = ["air_temperature_2m".to_string()];
    let ask =
        |engine: &NetcdfEngine| answer(engine.query_position(&here, None, Some(&t2m), None, None));
    let through_dap = ask(&dap);
    assert_eq!(through_dap, ask(&local));
    let values = &through_dap.as_ref().unwrap()[0].1["air_temperature_2m"].1;
    assert_eq!(values.len(), 3, "the newest run's three leads");
    assert!(values.iter().all(Option::is_some));
    let reads = reads_of(&server.take(), "air_temperature_2m");
    let files: BTreeSet<&str> = reads.iter().map(|(file, _)| file.as_str()).collect();
    assert_eq!((reads.len(), files.len()), (3, 3), "{reads:#?}");
    // Columns 4-5 and rows 5-6 north first: stored rows 4-5 of 11.
    for (_, slabs) in &reads {
        assert_eq!(slabs, &["[0]", "[0]", "[4:1:5]", "[4:1:5]"]);
    }
    assert_eq!(ask(&dap), through_dap);
    assert!(
        server.take().is_empty(),
        "every window from the chunk cache"
    );
}

/// Windows answer what whole fields answer, value for value: a position
/// series at points inside the grid, at its edges, within and past the
/// half cell around it, at its corners and off it, for a variable served
/// converted (K to °C, 1 to %) and a per-step amount (its accumulation at
/// two steps); every read a window of at most 2 × 2. Several points share
/// one window per field; radius, area across the grid's edge, cube and
/// trajectory read windows too; and both runs, pinned, alike.
#[test]
fn windowed_queries_answer_what_whole_fields_answer() {
    let _pool = crate::remote::tests::query_pool_shared();
    let files = sfc_files();
    let (_dir, local) = local(&files, "sfc", SFC_VARIABLES);
    let server = TestServer::new();
    publish(&server, &files, true, None);
    let dap = opened(&config(&server, "sfc", &[0, 1, 2], SFC_VARIABLES));
    server.take();
    let parameters = [
        "air_temperature_2m",
        "relative_humidity_2m",
        "precipitation_amount",
    ]
    .map(String::from);
    let mut cells = Vec::new();
    for col in [-0.9, -0.4, 0.2, 4.5, 8.6, 9.3, 9.7] {
        for row in [-0.8, -0.3, 0.2, 5.5, 9.9, 10.2, 10.45] {
            cells.push((col, row));
        }
    }
    cells.extend([(-1.6, 3.0), (5.0, 11.7)]);
    let points = at_cells(&dap, &cells);
    let mut sampled = 0;
    for (&cell, &at) in cells.iter().zip(&points) {
        let ask = |engine: &NetcdfEngine| {
            answer(engine.query_position(&point(at), None, Some(&parameters), None, None))
        };
        let through_dap = ask(&dap);
        assert_eq!(through_dap, ask(&local), "at cell {cell:?}");
        if let Ok(coverages) = &through_dap {
            sampled += coverages[0].1.values().flat_map(|r| &r.1).flatten().count();
        }
    }
    assert!(sampled > 400, "{sampled} values");
    let requests = server.take();
    for variable in [
        "air_temperature_2m",
        "relative_humidity_2m",
        "precipitation_amount_acc",
    ] {
        let reads = reads_of(&requests, variable);
        assert!(reads.len() > 50, "{variable}: {} reads", reads.len());
        for (file, slabs) in &reads {
            let (rows, columns) = (span(&slabs[2]), span(&slabs[3]));
            assert!(rows <= 2 && columns <= 2, "{variable} of {file}: {slabs:?}");
        }
    }

    // Several points: one window per field, joining theirs.
    let inside: Vec<String> = at_cells(&dap, &[(1.2, 2.5), (3.8, 1.1), (2.4, 4.7)])
        .into_iter()
        .map(point)
        .collect();
    assert_eq!(
        answers(&dap, &inside, &parameters),
        answers(&local, &inside, &parameters)
    );
    let reads = reads_of(&server.take(), "air_temperature_2m");
    assert_eq!(reads.len(), 3, "one read per lead for every point");
    for (_, slabs) in &reads {
        // Columns 1-4, rows 1-5 north first: stored rows 5-9.
        assert_eq!(&slabs[2..], ["[5:1:9]", "[1:1:4]"]);
    }

    // Every shape, in both runs, pinned and not.
    let corner = at_cells(&dap, &[(-2.5, -2.5), (2.5, 3.0)]);
    let middle = at_cells(&dap, &[(3.0, 3.0), (6.4, 7.2)]);
    let (lon, lat) = middle[0];
    let bbox = Bbox::new(
        middle[0].0.min(middle[1].0),
        middle[0].1.min(middle[1].1),
        middle[0].0.max(middle[1].0),
        middle[0].1.max(middle[1].1),
    )
    .unwrap();
    let path = format!(
        "LINESTRING({} {}, {} {})",
        middle[0].0, middle[0].1, middle[1].0, middle[1].1
    );
    let runs = dap.raster_info().reference_times;
    for run in [None, Some(runs[0]), Some(runs[1])] {
        let shapes = |engine: &NetcdfEngine| {
            let p = Some(&parameters[..]);
            [
                answer(engine.query_radius(&point((lon, lat)), 300_000.0, None, p, None, run)),
                answer(engine.query_area(&polygon(&corner), None, p, None, run)),
                answer(engine.query_area(&polygon(&middle), None, p, None, run)),
                answer(engine.query_cube(&bbox, None, p, None, CubeResolution::default(), run)),
                answer(engine.query_trajectory(&path, None, p, None, run)),
            ]
        };
        let through_dap = shapes(&dap);
        assert_eq!(through_dap, shapes(&local), "run {run:?}");
        assert!(through_dap.iter().all(Result::is_ok), "{through_dap:?}");
    }
    let reads = reads_of(&server.take(), "air_temperature_2m");
    assert!(!reads.is_empty());
    for (file, slabs) in &reads {
        let cells = span(&slabs[2]) * span(&slabs[3]);
        assert!(cells * 2 <= 110, "{file}: {slabs:?} is a window");
    }
    let edge = reads
        .iter()
        .any(|(_, slabs)| slabs[2].ends_with(":10]") && slabs[3].starts_with("[0:"));
    assert!(
        edge,
        "the area across the corner reads to the grid's edge: {reads:#?}"
    );
}

/// A window of more than half the field is read whole (#1171,
/// `worth_a_window`): an area over the whole grid reads every field whole,
/// as before. A whole field a query left in the chunk cache is then
/// sampled whole by the next query, which fetches windows only of the
/// fields not there.
#[test]
fn an_area_over_most_of_the_grid_reads_whole_fields() {
    let _pool = crate::remote::tests::query_pool_shared();
    let files = sfc_files();
    let (_dir, local) = local(&files, "sfc", SFC_VARIABLES);
    let server = TestServer::new();
    publish(&server, &files, true, None);
    let dap = opened(&config(&server, "sfc", &[0, 1, 2], SFC_VARIABLES));
    server.take();
    let t2m = ["air_temperature_2m".to_string()];
    let [w, s, e, n] = dap.raster_info().spatial_extent.unwrap();
    let all = format!("POLYGON(({w} {s}, {e} {s}, {e} {n}, {w} {n}, {w} {s}))");
    let area =
        |engine: &NetcdfEngine| answer(engine.query_area(&all, None, Some(&t2m), None, None));
    assert_eq!(area(&dap), area(&local));
    let reads = reads_of(&server.take(), "air_temperature_2m");
    assert_eq!(reads.len(), 1, "the newest run's last step");
    assert_eq!(reads[0].1, ["[0]", "[0]", "[0:1:10]", "[0:1:9]"]);
    let whole = reads[0].0.clone();
    assert!(whole.starts_with("meps_sfc_02_"), "{whole}");
    // Most of the grid: a window of 10 rows by 8 columns would do, 80 of
    // the 110 cells, and the field is read whole (of the run before).
    let most = polygon(&at_cells(&dap, &[(0.0, 0.0), (7.5, 8.5)]));
    let runs = dap.raster_info().reference_times;
    let area = |engine: &NetcdfEngine| {
        answer(engine.query_area(&most, None, Some(&t2m), None, Some(runs[0])))
    };
    assert_eq!(area(&dap), area(&local));
    let reads = reads_of(&server.take(), "air_temperature_2m");
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].1, ["[0]", "[0]", "[0:1:10]", "[0:1:9]"]);
    assert_ne!(reads[0].0, whole);

    let here = point(at_cells(&dap, &[(4.3, 5.6)])[0]);
    let series =
        |engine: &NetcdfEngine| answer(engine.query_position(&here, None, Some(&t2m), None, None));
    assert_eq!(series(&dap), series(&local));
    let reads = reads_of(&server.take(), "air_temperature_2m");
    assert_eq!(reads.len(), 2, "{reads:#?}");
    for (file, slabs) in &reads {
        assert_ne!(file, &whole, "the cached whole field is sampled whole");
        assert_eq!(&slabs[2..], ["[4:1:5]", "[4:1:5]"]);
    }
}

/// Windows of a global lat/lon grid, stored north-up: packing and the fill
/// box of `t_packed` (the nearest cell where a neighbour is fill) and the
/// seam, where a point between the last column and the first reads a
/// window of every column, answer what the whole fields answer.
#[test]
fn windows_keep_packing_fill_and_the_global_seam() {
    let _pool = crate::remote::tests::query_pool_shared();
    let (_, late) = slots();
    let files = [("netcdf-global/global_5deg.nc", "global", 0, late)];
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(testdata(files[0].0), dir.path().join("global_5deg.nc")).unwrap();
    let local_config: NetcdfConfig = toml::from_str(&format!(
        "data_path = {:?}",
        dir.path().display().to_string()
    ))
    .unwrap();
    let local = NetcdfEngine::new("local", &local_config).unwrap();
    let server = TestServer::new();
    publish(&server, &files, true, None);
    let dap = opened(&config(&server, "global", &[0], ""));
    assert_eq!(grid(&dap), (72, 36));
    server.take();
    let parameters = ["air_temperature", "t_packed"].map(String::from);
    let points = [
        (1.0, 1.0),
        (5.0, 5.0),
        (9.0, 11.0),
        (-1.0, 4.0),
        (12.0, 6.0),
        (4.0, -1.0),
        (179.0, 20.0),
        (-179.0, -30.0),
        (180.0, 0.0),
        (0.0, 89.0),
        (100.0, -89.5),
    ];
    let mut filled = 0;
    for at in points {
        let ask = |engine: &NetcdfEngine| {
            answer(engine.query_position(&point(at), None, Some(&parameters), None, None))
        };
        let through_dap = ask(&dap);
        assert_eq!(through_dap, ask(&local), "at {at:?}");
        let coverages = through_dap.unwrap();
        filled += coverages[0].1["t_packed"].1.iter().flatten().count();
    }
    assert!(filled > 20, "{filled} values");
    let reads = reads_of(&server.take(), "t_packed");
    assert_eq!(reads.len(), points.len() * 3, "three steps per point");
    let seam = reads
        .iter()
        .filter(|(_, slabs)| slabs[2] == "[0:1:71]")
        .count();
    assert_eq!(
        seam,
        3 * 3,
        "the three seam points read every column: {reads:#?}"
    );
    assert!(
        reads.iter().all(|(_, slabs)| span(&slabs[1]) <= 2),
        "{reads:#?}"
    );
}

/// What thredds.met.no served for MEPS (testdata/netcdf-opendap): a point
/// query on its 949 × 1069 grid, stored south-up, asks for exactly the
/// window its cells take, in the constraint THREDDS answered (brackets
/// percent-encoded), and answers what the whole field gives there. Two
/// points read one window joining theirs, answered with THREDDS's own
/// 4 × 4 reply of 2 m temperature (stored rows 500-503, columns 400-403);
/// one point its 2 × 2, answered with that reply's cells, re-encoded.
#[test]
fn a_point_on_thredds_meps_reads_its_window() {
    let _pool = crate::remote::tests::query_pool_shared();
    let server = TestServer::new();
    let (_, late) = slots();
    let (dir, name) = place("sfc", 0, late);
    let key = format!("{DAP}/{dir}/{name}");
    let run: DateTime<Utc> = "2026-10-23T12:00:00Z".parse().unwrap();
    let captured = |file: &str| std::fs::read(testdata(&format!("netcdf-opendap/{file}"))).unwrap();
    let stem = "meps_sfc_00_20261023T12Z";
    server.put(
        &format!("{key}.dds"),
        captured(&format!("{stem}.nc.dds")),
        run,
    );
    server.put(
        &format!("{key}.das"),
        captured(&format!("{stem}.nc.das")),
        run,
    );
    server.put(
        &format!("{key}.dods?{SFC_COORDINATES}"),
        captured(&format!("{stem}_coordinates.dods")),
        run,
    );
    let four = captured(&format!("{stem}_t2m_4x4.dods"));
    let Raw::F32(kelvin) = dap::take(dap::decode(&four).unwrap(), "air_temperature_2m").unwrap()
    else {
        panic!("2 m temperature is Float32");
    };
    let field = "air_temperature_2m.air_temperature_2m%5B0%5D%5B0%5D";
    server.put(
        &format!("{key}.dods?{field}%5B500:1:503%5D%5B400:1:403%5D"),
        four,
        run,
    );
    // Stored rows 501-502 and columns 401-402: cells 5, 6, 9, 10 of 16.
    let mut xdr = [[0, 0, 0, 4], [0, 0, 0, 4]].concat();
    for i in [5, 6, 9, 10] {
        xdr.extend_from_slice(&kelvin[i].to_be_bytes());
    }
    let two = dods(
        "    Structure {\n        Float32 air_temperature_2m[time = 1][height1 = 1][y = 2][x = 2];\n    \
         } air_temperature_2m;\n",
        &xdr,
    );
    server.put(
        &format!("{key}.dods?{field}%5B501:1:502%5D%5B401:1:402%5D"),
        two,
        run,
    );
    catalogs(&server, &[(dir.clone(), name.clone(), 166_700_000)]);
    let config = config(
        &server,
        "sfc",
        &[0],
        r#"variables = ["air_temperature_2m"]"#,
    );
    let engine = NetcdfEngine::new("thredds", &config).unwrap();
    assert_eq!(grid(&engine), (949, 1069));
    assert_eq!(engine.raster_info().reference_times, [run]);
    server.take();

    // The whole field as a render would hold it: north-up, NaN but for the
    // captured 4 × 4, whose stored row 500 is row 568 north first.
    let catalog = engine.catalog.load_full();
    let layout = catalog.layout.as_ref().unwrap();
    let mut whole = vec![f32::NAN; 949 * 1069];
    for (i, &k) in kelvin.iter().enumerate() {
        let (row, col) = (1068 - (500 + i / 4), 400 + i % 4);
        whole[row * 949 + col] = k;
    }
    let expected = |(lon, lat): (f64, f64)| {
        let position = layout.source.position(lon, lat);
        let kelvin = layout.source.value_at(&whole, None, position).unwrap();
        Some(layout.to_display(0, kelvin))
    };
    let t2m = ["air_temperature_2m".to_string()];
    let value = |answer: &Answer| {
        answer.as_ref().unwrap()[0].1["air_temperature_2m"]
            .1
            .clone()
    };

    let two_points = at_cells(&engine, &[(400.3, 565.4), (402.7, 567.8)]);
    let asked: Vec<String> = two_points.iter().copied().map(point).collect();
    let answered = answers(&engine, &asked, &t2m);
    for (answer, &at) in answered.iter().zip(&two_points) {
        assert_eq!(value(answer), [expected(at)], "at {at:?}");
    }
    let requests: Vec<String> = dap_requests(&server.take());
    assert_eq!(
        requests,
        [format!(
            "{dir}/{name}.dods?{field}%5B500:1:503%5D%5B400:1:403%5D"
        )]
    );

    let one = at_cells(&engine, &[(401.4, 566.3)])[0];
    let answered = answer(engine.query_position(&point(one), None, Some(&t2m), None, None));
    let celsius = value(&answered)[0].unwrap();
    assert_eq!(Some(celsius), expected(one));
    assert!((0.0..15.0).contains(&celsius), "{celsius} °C");
    assert_eq!(
        dap_requests(&server.take()),
        [format!(
            "{dir}/{name}.dods?{field}%5B501:1:502%5D%5B401:1:402%5D"
        )]
    );
}

/// The 13-level fixture (testdata/netcdf-pressure-13: MEPS's levels stored
/// top first, rows south-up, one file of six steps) through OPeNDAP from a
/// server of its own, and read locally, whole.
fn thirteen_levels() -> (TestServer, NetcdfEngine, tempfile::TempDir, NetcdfEngine) {
    let (_, late) = slots();
    let files = [("netcdf-pressure-13/pressure_13_levels.nc", "pl13", 0, late)];
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        testdata(files[0].0),
        dir.path().join("pressure_13_levels.nc"),
    )
    .unwrap();
    let local_config: NetcdfConfig = toml::from_str(&format!(
        "data_path = {:?}",
        dir.path().display().to_string()
    ))
    .unwrap();
    let local = NetcdfEngine::new("local", &local_config).unwrap();
    let server = TestServer::new();
    publish(&server, &files, true, None);
    let dap = opened(&config(&server, "pl13", &[0], ""));
    assert_eq!(grid(&dap), (10, 8));
    assert_eq!(dap.raster_info().vertical.unwrap().levels.len(), 13);
    server.take();
    (server, dap, dir, local)
}

/// No cap.
const UNCAPPED: Limits = Limits {
    bytes: u64::MAX,
    reads: usize::MAX,
};

/// A position query of `parameters` at `at`, at `z` (every level without),
/// capped at `limits`.
fn position(
    engine: &NetcdfEngine,
    at: (f64, f64),
    parameters: &[String],
    z: Option<&[f64]>,
    limits: Limits,
) -> Result<CoverageResponse, DataServerError> {
    let mut answered = None;
    engine.positions(&[at], None, Some(parameters), z, None, limits, &mut |r| {
        answered = Some(r);
        Ok(())
    })?;
    Ok(answered.unwrap())
}

/// The pressure-axis hyperslab of each read among `reads`.
fn levels_read(reads: &[(String, Vec<String>)]) -> Vec<&str> {
    reads.iter().map(|(_, slabs)| slabs[1].as_str()).collect()
}

/// A position series at all 13 pressure levels over six steps is one
/// request per variable and step, not one per level: one hyperslab over the
/// 13 stored levels of the point's 2 × 2 cells, its rows read where they
/// are stored, south-up. Every level answers what the file read whole
/// answers, the fill cell's neighbour included. The byte cap counts what
/// the windows decode (12 reads of 2 × 2 cells × 13 levels × 4 bytes), so
/// a cap that refuses the same series read whole admits it; the read cap
/// counts each window of 13 levels once, and over it refuses before any
/// request. Asked again, every window comes from the chunk cache.
#[test]
fn a_series_at_every_level_is_one_request_per_variable_and_step() {
    let _pool = crate::remote::tests::query_pool_shared();
    let (server, dap, _dir, local) = thirteen_levels();
    let at = at_cells(&dap, &[(4.3, 2.6)])[0];
    let parameters = ["air_temperature_pl", "relative_humidity_pl"].map(String::from);
    let windows = 12 * 4 * 13 * 4;
    let ask =
        |engine: &NetcdfEngine, limits| answer(position(engine, at, &parameters, None, limits));

    let refused = ask(
        &dap,
        Limits {
            bytes: windows,
            reads: 11,
        },
    )
    .unwrap_err();
    assert!(
        refused.contains("12 field reads (6 timesteps × 2 source variables)")
            && refused.contains("the limit is 11 reads")
            && !refused.contains("level"),
        "levels read together are no reads more: {refused}"
    );
    let refused = ask(
        &dap,
        Limits {
            bytes: windows - 1,
            reads: 12,
        },
    )
    .unwrap_err();
    assert!(
        refused.contains("156 fields (6 timesteps × 13 levels × 2 source variables, 2496 bytes)"),
        "{refused}"
    );
    assert!(server.take().is_empty(), "refused before any request");

    let through_dap = ask(
        &dap,
        Limits {
            bytes: windows,
            reads: 12,
        },
    );
    let whole = ask(&local, UNCAPPED);
    assert_eq!(through_dap, whole);
    let coverages = through_dap.as_ref().unwrap();
    assert_eq!(coverages.len(), 6, "a vertical profile per step");
    for (_, ranges) in coverages {
        let (shape, values) = &ranges["air_temperature_pl"];
        assert_eq!(shape, &[13]);
        let distinct: BTreeSet<u64> = values.iter().map(|v| v.unwrap().to_bits()).collect();
        assert_eq!(distinct.len(), 13, "every level its own: {values:?}");
    }
    let requests = server.take();
    for variable in ["air_temperature_pl", "relative_humidity_pl"] {
        let reads = reads_of(&requests, variable);
        let steps: BTreeSet<&str> = reads.iter().map(|(_, slabs)| slabs[0].as_str()).collect();
        assert_eq!((reads.len(), steps.len()), (6, 6), "{variable}: {reads:#?}");
        // Columns 4-5 and rows 2-3 north first: stored rows 4-5 of 8.
        for (_, slabs) in &reads {
            assert_eq!(&slabs[1..], ["[0:1:12]", "[4:1:5]", "[4:1:5]"]);
        }
    }
    let refused = ask(
        &local,
        Limits {
            bytes: windows,
            reads: 12,
        },
    )
    .unwrap_err();
    assert!(
        refused.contains("156 fields (6 timesteps × 13 levels × 2 source variables, 49920 bytes)"),
        "the same series read whole: {refused}"
    );
    assert_eq!(
        ask(
            &dap,
            Limits {
                bytes: windows,
                reads: 12
            }
        ),
        whole
    );
    assert!(
        server.take().is_empty(),
        "every window from the chunk cache"
    );
}

/// `z` of two levels far apart (1000 and 500 hPa, stored 12 and 7) is one
/// hyperslab over the stored levels between them, at a point, each level
/// answering its own values in the order `z` names them; the other order
/// reads the same hyperslabs, from the chunk cache. Over a window too wide
/// for the levels between to be cheap, the two levels are read one by one,
/// each its own window.
#[test]
fn levels_far_apart_are_read_together_while_the_window_is_small() {
    let _pool = crate::remote::tests::query_pool_shared();
    let (server, dap, _dir, local) = thirteen_levels();
    let t = ["air_temperature_pl".to_string()];
    let at = at_cells(&dap, &[(6.5, 4.2)])[0];
    for z in [[500.0, 1000.0], [1000.0, 500.0]] {
        let ask = |engine: &NetcdfEngine| answer(position(engine, at, &t, Some(&z), UNCAPPED));
        let through_dap = ask(&dap);
        assert_eq!(through_dap, ask(&local), "z {z:?}");
        for (_, ranges) in through_dap.unwrap() {
            let values = &ranges["air_temperature_pl"].1;
            let at_1000 = values[z.iter().position(|&l| l == 1000.0).unwrap()].unwrap();
            let at_500 = values[z.iter().position(|&l| l == 500.0).unwrap()].unwrap();
            assert!(
                ((at_500 - at_1000) - 25.0).abs() < 1e-3,
                "0.05 K per hPa: {values:?}"
            );
        }
        let reads = reads_of(&server.take(), "air_temperature_pl");
        if z[0] == 500.0 {
            assert_eq!(reads.len(), 6, "{reads:#?}");
            assert!(levels_read(&reads).iter().all(|&slab| slab == "[7:1:12]"));
        } else {
            assert!(reads.is_empty(), "the same hyperslabs, cached: {reads:#?}");
        }
    }
    // A quarter of the grid: 2 levels of 20 cells each, against 6 levels
    // of 20 in one hyperslab.
    let corners = at_cells(&dap, &[(1.0, 1.0), (4.6, 4.6)]);
    let area = polygon(&corners);
    let z = [1000.0, 500.0];
    let ask = |engine: &NetcdfEngine| {
        answer(engine.area(&area, None, Some(&t), Some(&z), None, UNCAPPED))
    };
    assert_eq!(ask(&dap), ask(&local));
    let reads = reads_of(&server.take(), "air_temperature_pl");
    let mut slabs = levels_read(&reads);
    slabs.sort_unstable();
    assert_eq!(slabs, ["[12]", "[7]"], "{reads:#?}");
    assert!(reads
        .iter()
        .all(|(_, slabs)| span(&slabs[3]) * span(&slabs[2]) * 2 <= 80));
}

/// A Z path samples each level at its own samples. A short one's levels
/// at a step are one read: the window joining their cells stays small. A
/// path across the grid joins to more than half of it, and its levels are
/// read one by one, each the window of its own samples. Both answer what
/// the file read whole answers.
#[test]
fn a_z_path_reads_its_levels_together_while_their_window_is_small() {
    let _pool = crate::remote::tests::query_pool_shared();
    let (server, dap, _dir, local) = thirteen_levels();
    let t = ["air_temperature_pl".to_string()];
    let path = |from: (f64, f64), to: (f64, f64), z: (f64, f64)| {
        let ends = at_cells(&dap, &[from, to]);
        format!(
            "LINESTRING Z({} {} {}, {} {} {})",
            ends[0].0, ends[0].1, z.0, ends[1].0, ends[1].1, z.1
        )
    };
    let ask = |engine: &NetcdfEngine, path: &str| {
        answer(engine.trajectory(path, None, Some(&t), None, None, UNCAPPED))
    };

    let short = path((3.2, 3.3), (4.6, 3.8), (1000.0, 850.0));
    let through_dap = ask(&dap, &short);
    assert_eq!(through_dap, ask(&local, &short));
    assert!(through_dap
        .unwrap()
        .iter()
        .all(|(_, ranges)| ranges["air_temperature_pl"].1.iter().all(Option::is_some)));
    let reads = reads_of(&server.take(), "air_temperature_pl");
    assert_eq!(reads.len(), 6, "one read per step: {reads:#?}");
    for slab in levels_read(&reads) {
        assert!(
            slab.contains(":1:") && slab.ends_with(":12]"),
            "1000 hPa up to 850 at least: {slab}"
        );
    }

    let long = path((0.2, 0.3), (9.4, 6.6), (1000.0, 300.0));
    let through_dap = ask(&dap, &long);
    assert_eq!(through_dap, ask(&local, &long));
    let reads = reads_of(&server.take(), "air_temperature_pl");
    let levels: BTreeSet<&str> = levels_read(&reads).into_iter().collect();
    assert!(
        levels.len() > 3 && levels.iter().all(|slab| !slab.contains(':')),
        "each level read alone: {levels:?}"
    );
    assert_eq!(reads.len(), 6 * levels.len(), "{reads:#?}");
    for (file, slabs) in &reads {
        let cells = span(&slabs[2]) * span(&slabs[3]);
        assert!(cells * 2 <= 80, "{file}: {slabs:?} is a window");
    }
}

/// Read whole, a field counts its bytes: an area over the whole grid at
/// every level reads the 13 levels' fields whole, one read each (a window
/// of them would pass half of what they hold), and both caps count them so.
#[test]
fn whole_fields_count_whole() {
    let _pool = crate::remote::tests::query_pool_shared();
    let (server, dap, _dir, local) = thirteen_levels();
    let t = ["air_temperature_pl".to_string()];
    let [w, s, e, n] = dap.raster_info().spatial_extent.unwrap();
    let all = format!("POLYGON(({w} {s}, {e} {s}, {e} {n}, {w} {n}, {w} {s}))");
    let ask = |engine: &NetcdfEngine, limits| {
        answer(engine.area(&all, None, Some(&t), None, None, limits))
    };
    let fields = 13 * 80 * 4;
    let refused = ask(
        &dap,
        Limits {
            bytes: fields - 1,
            reads: 13,
        },
    )
    .unwrap_err();
    assert!(
        refused.contains("13 fields (1 timesteps × 13 levels × 1 source variables, 4160 bytes)"),
        "{refused}"
    );
    let refused = ask(
        &dap,
        Limits {
            bytes: fields,
            reads: 12,
        },
    )
    .unwrap_err();
    assert!(
        refused.contains("13 field reads (1 timesteps × 13 levels × 1 source variables)")
            && refused.contains("fewer pressure levels (z)"),
        "{refused}"
    );
    assert!(server.take().is_empty());
    assert_eq!(
        ask(
            &dap,
            Limits {
                bytes: fields,
                reads: 13
            }
        ),
        ask(&local, UNCAPPED)
    );
    let reads = reads_of(&server.take(), "air_temperature_pl");
    assert_eq!(reads.len(), 13);
    for (_, slabs) in &reads {
        assert!(!slabs[1].contains(':'));
        assert_eq!(&slabs[2..], ["[0:1:7]", "[0:1:9]"]);
    }
}

/// A read of 13 levels at once that outlives its query's deadline goes on
/// and lands in the chunk cache, one entry per hyperslab: retries join the
/// reads still running and decode the ones landed, and the series is
/// answered having made the first query's requests only, one per step.
#[test]
fn a_read_of_every_level_past_its_deadline_lands_in_the_cache() {
    let _pool = crate::remote::tests::query_pool();
    let (server, dap, _dir, local) = thirteen_levels();
    server.slow_over(0, Duration::from_millis(800));
    let t = ["air_temperature_pl".to_string()];
    let at = at_cells(&dap, &[(5.5, 5.5)])[0];
    let within = |ms: u64| {
        let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(ms)));
        position(&dap, at, &t, None, UNCAPPED)
    };
    let started = Instant::now();
    let late = within(100);
    assert!(
        matches!(late, Err(DataServerError::DeadlineExceeded)),
        "{late:?}"
    );
    let answered = loop {
        match within(300) {
            Ok(answered) => break answered,
            Err(DataServerError::DeadlineExceeded) => {
                assert!(started.elapsed() < Duration::from_secs(10), "never cached");
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(
        answer(Ok(answered)),
        answer(position(&local, at, &t, None, UNCAPPED))
    );
    let reads = reads_of(&server.take(), "air_temperature_pl");
    assert_eq!(
        reads.len(),
        6,
        "the first query's requests only: {reads:#?}"
    );
    assert!(levels_read(&reads).iter().all(|&slab| slab == "[0:1:12]"));
}
