//! The THREDDS catalog parser on MET Norway's own catalog of one MEPS day
//! (testdata/netcdf-thredds-catalog), and the catalog prober over a
//! scripted store: the same files as the template route, the walk bounded
//! to the window, settling, replaced files and failures.
//!
//! The catalogs above and below the captured day are written with
//! `quick_xml::Writer`, never `format!` (Critical Rule 3).

use std::sync::Arc;

use quick_xml::events::{BytesEnd, BytesRef, BytesStart, BytesText, Event};
use quick_xml::Writer;

use super::*;
use crate::discovery::{Schedule, TimeWindow, SETTLED_RECHECK};
use crate::test_store::MetaStore;

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// The captured catalog of `meps25epsarchive/2026/10/19/`.
fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/netcdf-thredds-catalog/meps25epsarchive_2026_10_19_catalog.xml"
    ))
    .unwrap()
}

/// thredds.met.no's layout on a test origin: catalogs under
/// `thredds/catalog/`, files under `thredds/fileServer/`.
const ORIGIN: &str = "https://thredds.test";
const CATALOGS: &str = "thredds/catalog/meps25epsarchive";
const FILES: &str = "thredds/fileServer/meps25epsarchive";

fn catalog_url() -> String {
    [ORIGIN, CATALOGS, "catalog.xml"].join("/")
}

fn files_url() -> String {
    [ORIGIN, FILES, ""].join("/")
}

/// The store key of the catalog of `dir` (`""` for the root).
fn catalog_key(dir: &str) -> String {
    [CATALOGS, dir, "catalog.xml"]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// A file a written catalog lists.
struct Entry<'a> {
    url_path: String,
    size: &'a str,
    units: &'a str,
    modified: &'a str,
}

fn entry<'a>(url_path: &str, size: &'a str, units: &'a str, modified: &'a str) -> Entry<'a> {
    Entry {
        url_path: url_path.to_string(),
        size,
        units,
        modified,
    }
}

fn start(writer: &mut Writer<Vec<u8>>, name: &str, attributes: &[(&str, &str)]) {
    let element = BytesStart::new(name).with_attributes(attributes.iter().copied());
    writer.write_event(Event::Start(element)).unwrap();
}

fn empty(writer: &mut Writer<Vec<u8>>, name: &str, attributes: &[(&str, &str)]) {
    let element = BytesStart::new(name).with_attributes(attributes.iter().copied());
    writer.write_event(Event::Empty(element)).unwrap();
}

fn end(writer: &mut Writer<Vec<u8>>, name: &str) {
    writer.write_event(Event::End(BytesEnd::new(name))).unwrap();
}

fn text(writer: &mut Writer<Vec<u8>>, name: &str, attributes: &[(&str, &str)], value: &str) {
    start(writer, name, attributes);
    writer
        .write_event(Event::Text(BytesText::new(value)))
        .unwrap();
    end(writer, name);
}

/// A catalog in thredds.met.no's shape: the services (with an `HTTPServer`
/// one when `file_server`), then one container dataset holding `files` and
/// a `catalogRef` to each of `refs`.
fn catalog_xml(refs: &[&str], files: &[Entry], file_server: bool) -> Vec<u8> {
    let mut w = Writer::new(Vec::new());
    start(
        &mut w,
        "catalog",
        &[
            (
                "xmlns",
                "http://www.unidata.ucar.edu/namespaces/thredds/InvCatalog/v1.0",
            ),
            ("xmlns:xlink", "http://www.w3.org/1999/xlink"),
            ("version", "1.2"),
        ],
    );
    start(
        &mut w,
        "service",
        &[("name", "all"), ("serviceType", "Compound"), ("base", "")],
    );
    empty(
        &mut w,
        "service",
        &[
            ("name", "dap"),
            ("serviceType", "OpenDAP"),
            ("base", "/thredds/dodsC/"),
        ],
    );
    if file_server {
        empty(
            &mut w,
            "service",
            &[
                ("name", "http"),
                ("serviceType", "HTTPServer"),
                ("base", "/thredds/fileServer/"),
            ],
        );
    }
    end(&mut w, "service");
    start(&mut w, "dataset", &[("name", "dir/")]);
    for file in files {
        let name = file.url_path.rsplit('/').next().unwrap();
        start(
            &mut w,
            "dataset",
            &[
                ("name", name),
                ("ID", &file.url_path),
                ("urlPath", &file.url_path),
            ],
        );
        text(&mut w, "dataSize", &[("units", file.units)], file.size);
        text(&mut w, "date", &[("type", "modified")], file.modified);
        end(&mut w, "dataset");
    }
    for href in refs {
        let title = href.trim_end_matches("/catalog.xml");
        empty(
            &mut w,
            "catalogRef",
            &[
                ("xlink:href", href),
                ("xlink:title", title),
                ("name", title),
            ],
        );
    }
    end(&mut w, "dataset");
    end(&mut w, "catalog");
    w.into_inner()
}

/// The captured day catalog, read whole.
#[test]
fn met_norways_day_catalog_parses() {
    let catalog = parse(&fixture()).unwrap();
    assert_eq!(
        catalog.services.len(),
        20,
        "five compounds and what they hold"
    );
    assert_eq!(catalog.file_server_base(), Some("/thredds/fileServer/"));
    assert_eq!(catalog.datasets.len(), 48);
    let first = &catalog.datasets[0];
    assert_eq!(
        first,
        &Dataset {
            name: "meps_det_2_5km_20261019T00Z.nc".into(),
            url_path: "meps25epsarchive/2026/10/19/meps_det_2_5km_20261019T00Z.nc".into(),
            size: Some(71_560_000_000),
            modified: Some(at("2026-10-19T04:05:37Z")),
        }
    );
    let ncml = catalog
        .datasets
        .iter()
        .find(|d| d.name == "meps_det_sfc_20261019T09Z.ncml")
        .unwrap();
    assert_eq!(ncml.size, Some(8_119), "8.119 Kbytes");
    assert!(catalog
        .datasets
        .iter()
        .all(|d| d.size.is_some() && d.modified.is_some()));
    let hours: Vec<String> = (0..24).map(|h| format!("{h:02}/catalog.xml")).collect();
    let refs: Vec<&str> = catalog.refs.iter().map(|r| r.href.as_str()).collect();
    assert_eq!(refs, hours);
    assert_eq!(catalog.refs[9].title.as_deref(), Some("09"));
}

/// Entity references survive: in attributes (unescaped whole) and in
/// `dataSize` and `date` text, where quick-xml ≥ 0.38 splits them into
/// `GeneralRef` events of their own. A parser keeping only `Text` would
/// read `2026-10-19T0405:37Z` and `15` here.
#[test]
fn entity_references_survive_in_text_and_attributes() {
    let mut w = Writer::new(Vec::new());
    start(&mut w, "catalog", &[]);
    start(
        &mut w,
        "dataset",
        &[("name", "a&b <1>.nc"), ("urlPath", "dir/a&b <1>.nc")],
    );
    start(&mut w, "dataSize", &[("units", "Mbytes")]);
    w.write_event(Event::Text(BytesText::new("1"))).unwrap();
    w.write_event(Event::GeneralRef(BytesRef::new("#46")))
        .unwrap();
    w.write_event(Event::Text(BytesText::new("5"))).unwrap();
    end(&mut w, "dataSize");
    start(&mut w, "date", &[("type", "modified")]);
    w.write_event(Event::Text(BytesText::new("2026-10-19T04")))
        .unwrap();
    w.write_event(Event::GeneralRef(BytesRef::new("#58")))
        .unwrap();
    w.write_event(Event::Text(BytesText::new("05:37Z")))
        .unwrap();
    end(&mut w, "date");
    end(&mut w, "dataset");
    empty(&mut w, "catalogRef", &[("xlink:href", "x&y/catalog.xml")]);
    end(&mut w, "catalog");
    let xml = w.into_inner();
    assert!(String::from_utf8_lossy(&xml).contains("a&amp;b &lt;1&gt;.nc"));

    let catalog = parse(&xml).unwrap();
    assert_eq!(
        catalog.datasets,
        [Dataset {
            name: "a&b <1>.nc".into(),
            url_path: "dir/a&b <1>.nc".into(),
            size: Some(1_500_000),
            modified: Some(at("2026-10-19T04:05:37Z")),
        }]
    );
    assert_eq!(catalog.refs[0].href, "x&y/catalog.xml");
}

/// THREDDS's units, in powers of 1000, any case; anything else is no size.
#[test]
fn data_sizes_and_their_units() {
    for (value, units, bytes) in [
        ("71.56", "Gbytes", Some(71_560_000_000)),
        ("8.052", "Kbytes", Some(8_052)),
        ("167.8", "Mbytes", Some(167_800_000)),
        ("1.2", "Tbytes", Some(1_200_000_000_000)),
        ("512", "bytes", Some(512)),
        ("512", "", Some(512)),
        (" 3.5 ", "KBYTES", Some(3_500)),
        ("0", "Mbytes", Some(0)),
        ("1.5", "KiB", None),
        ("1.5", "furlongs", None),
        ("-1", "Kbytes", None),
        ("NaN", "Kbytes", None),
        ("inf", "Kbytes", None),
        ("twelve", "Kbytes", None),
    ] {
        assert_eq!(data_size(value, units), bytes, "{value} {units}");
    }
}

#[test]
fn modification_dates() {
    for (text, date) in [
        ("2026-10-19T04:05:37Z", Some("2026-10-19T04:05:37Z")),
        ("2026-10-19T06:05:37+02:00", Some("2026-10-19T04:05:37Z")),
        ("2026-10-19T04:05:37.250Z", Some("2026-10-19T04:05:37.250Z")),
        ("2026-10-19T04:05:37", Some("2026-10-19T04:05:37Z")),
        ("2026-10-19", Some("2026-10-19T00:00:00Z")),
        ("yesterday", None),
    ] {
        assert_eq!(parse_date(text), date.map(at), "{text}");
    }
}

/// What a server answers instead of a catalog is an error, never an empty
/// listing.
#[test]
fn anything_but_a_catalog_is_an_error() {
    let html = b"<html><body><h1>404</h1></body></html>";
    let error = parse(html).unwrap_err().to_string();
    assert!(error.contains("root element is <html>"), "{error}");
    assert!(parse(b"<catalog><dataset name=\"a\"></catalog>").is_err());
    assert!(parse(b"").unwrap_err().to_string().contains("no root"));
    assert!(parse(&[0xff, 0xfe, b'<']).is_err());
    let huge = vec![b' '; MAX_CATALOG_BYTES + 1];
    assert!(parse(&huge).unwrap_err().to_string().contains("limit"));
}

#[test]
fn catalog_url_names_a_catalog_document() {
    let new = |url: &str| CatalogProber::new(sfc_spec(&[0], &[6], "-PT6H"), url, &files_url());
    new(&catalog_url()).unwrap();
    for (url, why) in [
        (
            "https://thredds.test/thredds/catalog/x/catalog.html",
            ".xml",
        ),
        (
            "https://thredds.test/thredds/catalog/x/catalog.xml?a=1",
            "query",
        ),
        (
            "https://thredds.test/thredds/catalog/x/catalog.xml#top",
            "fragment",
        ),
        ("ftp://thredds.test/catalog.xml", "http(s)"),
        ("thredds/catalog.xml", "relative URL"),
    ] {
        let error = new(url).unwrap_err().to_string();
        assert!(
            error.contains("catalog_url") && error.contains(why),
            "{url}: {error}"
        );
    }
}

/// MEPS per-lead surface files of the control member, as the example
/// configures them.
fn sfc_spec(leads: &[u32], run_hours: &[u32], window: &str) -> TemplateSpec {
    TemplateSpec {
        prefix_pattern: "%Y/%m/%d/{run}/member_00/".into(),
        leads: leads.to_vec(),
        time_window: Some(TimeWindow::parse(window).unwrap()),
        ..TemplateSpec::new(
            FILES,
            "meps_sfc_{lead}_%Y%m%dT{run}Z.nc",
            Schedule::Runs {
                run_hours: run_hours.to_vec(),
            },
            "meps",
        )
    }
}

/// `meps25epsarchive/<path>` as a catalog's `urlPath`.
fn url_path(path: &str) -> String {
    ["meps25epsarchive", path].join("/")
}

/// The sfc file of `lead` of the 2026-10-19 run at `hour`, relative to
/// the archive.
fn sfc(hour: u32, lead: u32) -> String {
    format!("2026/10/19/{hour:02}/member_00/meps_sfc_{lead:02}_20261019T{hour:02}Z.nc")
}

/// The key a file of the archive is read at.
fn file_key(path: &str) -> String {
    [FILES, path].join("/")
}

/// A store with the catalogs from the archive root down to the captured
/// day, which names every hour of 2026-10-19.
fn store_to_the_day() -> (Arc<MetaStore>, DataStore) {
    let meta = Arc::new(MetaStore::default());
    meta.put_body(
        &catalog_key(""),
        catalog_xml(&["2025/catalog.xml", "2026/catalog.xml"], &[], true),
    );
    for year in ["2025", "2026"] {
        meta.put_body(
            &catalog_key(year),
            catalog_xml(&["09/catalog.xml", "10/catalog.xml"], &[], true),
        );
    }
    meta.put_body(
        &catalog_key("2026/10"),
        catalog_xml(&["18/catalog.xml", "19/catalog.xml"], &[], true),
    );
    meta.put_body(&catalog_key("2026/10/19"), fixture());
    let store = DataStore::new(meta.clone());
    (meta, store)
}

/// Put the catalog of hour `hour` of 2026-10-19, with `member_00` (and
/// `member_01`, which nothing must read), and member 0's sfc `leads`,
/// modified at `modified`, plus a pl file of another family.
fn put_run(meta: &MetaStore, hour: u32, leads: &[u32], modified: &str) {
    let dir = format!("2026/10/19/{hour:02}");
    meta.put_body(
        &catalog_key(&dir),
        catalog_xml(
            &["member_00/catalog.xml", "member_01/catalog.xml"],
            &[],
            true,
        ),
    );
    let mut files: Vec<Entry> = leads
        .iter()
        .map(|&lead| entry(&url_path(&sfc(hour, lead)), "167.8", "Mbytes", modified))
        .collect();
    let pl = format!("{dir}/member_00/meps_pl_00_20261019T{hour:02}Z.nc");
    files.push(entry(&url_path(&pl), "42.10", "Mbytes", modified));
    for member in ["member_00", "member_01"] {
        meta.put_body(
            &catalog_key(&[dir.as_str(), member].join("/")),
            catalog_xml(&[], &files, true),
        );
    }
    for &lead in leads {
        // The exact size the fileServer's HEAD answers.
        meta.set(&file_key(&sfc(hour, lead)), 167_812_345, Some(at(modified)));
    }
}

fn prober(spec: TemplateSpec) -> CatalogProber {
    CatalogProber::new(spec, &catalog_url(), &files_url()).unwrap()
}

fn keys(files: &[ProbedFile]) -> Vec<String> {
    files.iter().map(|file| file.key.clone()).collect()
}

/// The issue's acceptance test (#1170): over MET Norway's own day catalog,
/// the catalog route lists exactly the files the template route finds by
/// HEAD, the per-run `meps_det_2_5km` family of 2026-10-19, with the same
/// slots and the same versions, the HEADs' exact ones, not the catalog's
/// rounded `71.56 Gbytes`. The last, 21Z, was modified at 00:35 the next
/// day; the next day's runs are not out: the month catalog names no day
/// 20, so no directory below it is read.
#[tokio::test(flavor = "multi_thread")]
async fn the_catalog_and_template_routes_find_the_same_files() {
    let (meta, store) = store_to_the_day();
    let day = parse(&fixture()).unwrap();
    let det: Vec<&Dataset> = day
        .datasets
        .iter()
        .filter(|d| d.name.starts_with("meps_det_2_5km_"))
        .collect();
    assert_eq!(det.len(), 8);
    for (i, dataset) in det.iter().enumerate() {
        let size = dataset.size.unwrap() + 1_234 + i as u64;
        let key = ["thredds/fileServer", dataset.url_path.as_str()].join("/");
        meta.set(&key, size, dataset.modified);
    }
    let spec = TemplateSpec {
        prefix_pattern: "%Y/%m/%d/".into(),
        time_window: Some(TimeWindow::parse("-PT27H").unwrap()),
        ..TemplateSpec::new(
            FILES,
            "meps_det_2_5km_%Y%m%dT{run}Z.nc",
            Schedule::Runs {
                run_hours: (0..24).step_by(3).collect(),
            },
            "meps-det",
        )
    };
    let now = at("2026-10-20T03:00:00Z");

    let mut template = TemplateProber::new(spec.clone()).unwrap();
    let report = template.poll(&store, now).unwrap();
    assert_eq!(
        (report.checked, report.heads),
        (10, 10),
        "8 runs, 20T00, 20T03"
    );
    meta.take_headed();
    meta.take_fetched();

    let mut catalog = prober(spec);
    let report = catalog.poll(&store, &store, now).unwrap();
    assert_eq!(report.checked, 10);
    assert_eq!(
        meta.take_fetched(),
        [
            catalog_key("2026/10/19"),
            catalog_key("2026/10"),
            catalog_key("2026"),
            catalog_key(""),
        ],
        "the walk reads only the way to 2026/10/19"
    );
    assert_eq!(report.catalogs, 4);
    assert_eq!(report.heads, 8, "one HEAD per file listed, for its version");
    assert_eq!(report.added.len(), 8);

    let found = template.files();
    assert_eq!(found.len(), 8);
    assert_eq!(catalog.files(), found);
    assert_eq!(
        found[0].key,
        "thredds/fileServer/meps25epsarchive/2026/10/19/meps_det_2_5km_20261019T00Z.nc"
    );
    assert_eq!(found[0].time, at("2026-10-19T00:00:00Z"));
    assert_eq!(found[0].version.size, 71_560_001_234);
    assert_eq!(
        found[0].version.last_modified,
        Some(at("2026-10-19T04:05:37Z"))
    );

    // Nothing is due again until the recheck but the sentinels of 20T00
    // and 20T03, whose walk ends at the month: one catalog, no HEAD.
    let report = catalog
        .poll(&store, &store, now + chrono::Duration::minutes(5))
        .unwrap();
    assert_eq!((report.checked, report.catalogs, report.heads), (2, 1, 0));
    assert_eq!(meta.take_fetched(), [catalog_key("2026/10")]);
}

/// A poll reads the catalogs on the way to the runs in its window and
/// nothing else: of the day's 24 hour refs only the two runs' hours, of
/// each run only `member_00`, never a ref to another origin, outside the
/// root or back up. The next poll reads the run directories it found
/// directly.
#[tokio::test(flavor = "multi_thread")]
async fn refs_are_followed_toward_the_window_only() {
    let (meta, store) = store_to_the_day();
    for hour in [3, 6, 9, 12] {
        put_run(&meta, hour, &[0, 1], "2026-10-19T10:00:00Z");
    }
    // The 06 run directory's catalog names refs that must not be followed.
    meta.put_body(
        &catalog_key("2026/10/19/06"),
        catalog_xml(
            &[
                "member_00/catalog.xml",
                "member_01/catalog.xml",
                "https://elsewhere.test/thredds/catalog/meps25epsarchive/2026/10/19/06/member_00/catalog.xml",
                "/thredds/catalog/other/2026/10/19/06/member_00/catalog.xml",
                "../catalog.xml",
            ],
            &[],
            true,
        ),
    );
    let mut prober = prober(sfc_spec(&[0, 1], &[6, 9], "-PT6H"));
    let now = at("2026-10-19T11:00:00Z");

    let report = prober.poll(&store, &store, now).unwrap();
    let mut walked = [
        "",
        "2026",
        "2026/10",
        "2026/10/19",
        "2026/10/19/06",
        "2026/10/19/06/member_00",
        "2026/10/19/09",
        "2026/10/19/09/member_00",
    ]
    .map(catalog_key)
    .to_vec();
    walked.sort();
    assert_eq!(meta.take_fetched(), walked);
    assert_eq!(report.checked, 2, "each run's sentinel");
    assert!(prober.files().is_empty(), "lead 01 is checked next poll");

    let report = prober.poll(&store, &store, now).unwrap();
    assert_eq!(
        meta.take_fetched(),
        ["2026/10/19/06/member_00", "2026/10/19/09/member_00"].map(catalog_key),
        "found directories are read directly"
    );
    assert_eq!((report.checked, report.heads), (2, 4));
    let mut expected: Vec<String> = [(6, 0), (6, 1), (9, 0), (9, 1)]
        .into_iter()
        .map(|(hour, lead)| file_key(&sfc(hour, lead)))
        .collect();
    expected.sort();
    let mut found = keys(&prober.files());
    found.sort();
    assert_eq!(found, expected);
    assert!(prober.files().iter().all(|f| f.version.size == 167_812_345));
    assert_eq!(meta.take_headed(), expected);
}

/// One poll reads at most its cap of catalogs; the keys beyond fail and
/// keep their state, and the next poll goes on from the directories found.
#[tokio::test(flavor = "multi_thread")]
async fn a_poll_reads_at_most_its_cap() {
    let (meta, store) = store_to_the_day();
    put_run(&meta, 9, &[0], "2026-10-19T10:00:00Z");
    let mut prober = prober(sfc_spec(&[0], &[9], "-PT3H"));
    prober.walk.max_catalogs = 3;
    let now = at("2026-10-19T11:00:00Z");

    let report = prober.poll(&store, &store, now).unwrap();
    assert_eq!((report.catalogs, report.checked), (3, 1));
    assert_eq!(report.failed, 0, "a deferred lookup is no failed HEAD");
    assert!(!report.unreachable());
    assert!(prober.files().is_empty());
    assert_eq!(meta.take_fetched().len(), 3);

    let report = prober.poll(&store, &store, now).unwrap();
    assert_eq!(report.catalogs, 3, "the day, the hour, the member");
    assert_eq!(keys(&prober.files()), [file_key(&sfc(9, 0))]);
}

/// The catalog's version settles a file by the shared `PendingFile` rule:
/// a recent file waits for two more polls that see it unchanged, a change
/// of its size or date restarts the count, and one HEAD then gives its
/// version. A file older than `min_age` settles at once (above).
#[tokio::test(flavor = "multi_thread")]
async fn files_settle_on_the_catalogs_versions() {
    let (meta, store) = store_to_the_day();
    let t0 = at("2026-10-19T10:00:00Z");
    let minute = |n: i64| t0 + chrono::Duration::minutes(n);
    let member = catalog_key("2026/10/19/09/member_00");
    let listing = |size: &str, modified: &str| {
        catalog_xml(
            &[],
            &[entry(&url_path(&sfc(9, 0)), size, "Mbytes", modified)],
            true,
        )
    };
    put_run(&meta, 9, &[0], "2026-10-19T09:59:00Z");
    meta.put_body(&member, listing("60.10", "2026-10-19T09:59:00Z"));
    let mut prober = prober(sfc_spec(&[0], &[9], "-PT3H"));

    for n in [0, 1] {
        let report = prober.poll(&store, &store, minute(n)).unwrap();
        assert!(report.added.is_empty(), "minute {n}");
    }
    // Still being written: grown, and modified again.
    meta.put_body(&member, listing("167.8", "2026-10-19T10:01:30Z"));
    for n in [2, 3] {
        let report = prober.poll(&store, &store, minute(n)).unwrap();
        assert!(report.added.is_empty(), "minute {n}");
        assert_eq!(report.heads, 0);
    }
    let report = prober.poll(&store, &store, minute(4)).unwrap();
    assert_eq!(report.heads, 1);
    assert_eq!(keys(&report.added), [file_key(&sfc(9, 0))]);
    assert_eq!(report.added[0].version.size, 167_812_345);
    for n in [5, 6] {
        let report = prober.poll(&store, &store, minute(n)).unwrap();
        assert_eq!((report.catalogs, report.heads), (0, 0), "minute {n}");
    }
}

/// A file replaced on the server: found at its run's due recheck through
/// the catalog's date, or at once when a reader asks, and HEADed again.
/// What left the listing is reported with the version it was listed with.
#[tokio::test(flavor = "multi_thread")]
async fn a_replaced_file_is_headed_again() {
    let (meta, store) = store_to_the_day();
    let key = file_key(&sfc(9, 0));
    put_run(&meta, 9, &[0], "2026-10-19T09:30:00Z");
    let mut prober = prober(sfc_spec(&[0], &[9], "-PT3H"));
    let t0 = at("2026-10-19T10:00:00Z");
    prober.poll(&store, &store, t0).unwrap();
    let old = prober.files()[0].clone();
    meta.take_headed();

    // Republished: a new date in the catalog and a new size on the server.
    put_run(&meta, 9, &[0], "2026-10-19T09:40:00Z");
    meta.set(&key, 167_899_999, Some(at("2026-10-19T09:40:00Z")));
    let report = prober.poll(&store, &store, t0).unwrap();
    assert_eq!((report.catalogs, report.heads), (0, 0), "not due yet");
    let report = prober.poll(&store, &store, t0 + SETTLED_RECHECK).unwrap();
    assert_eq!(report.withdrawn, [old]);
    assert_eq!(keys(&report.added), [key.as_str()]);
    assert_eq!(prober.files()[0].version.size, 167_899_999);
    assert_eq!(meta.take_headed(), [key.as_str()]);

    // Replaced again, the catalog not yet showing it: a reader's recheck
    // HEADs the file on the next poll all the same.
    let listed = prober.files()[0].clone();
    meta.set(&key, 167_000_001, Some(at("2026-10-19T09:40:00Z")));
    assert!(prober.recheck(&key));
    let report = prober.poll(&store, &store, t0 + SETTLED_RECHECK).unwrap();
    assert_eq!(report.heads, 1);
    assert_eq!(report.withdrawn, [listed]);
    assert_eq!(prober.files()[0].version.size, 167_000_001);
    assert!(!prober.recheck("thredds/fileServer/elsewhere.nc"));
}

/// A catalog that cannot be read fails its files, which keep their state,
/// and its remembered URL is forgotten: the next poll walks to it again.
/// Every request failing is an unreachable server.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_catalog_is_walked_to_again_and_all_failing_is_unreachable() {
    let (meta, store) = store_to_the_day();
    put_run(&meta, 9, &[0], "2026-10-19T09:30:00Z");
    let mut prober = prober(sfc_spec(&[0], &[9], "-PT3H"));
    let t0 = at("2026-10-19T10:00:00Z");
    prober.poll(&store, &store, t0).unwrap();
    assert_eq!(prober.files().len(), 1);
    meta.take_fetched();

    let member = catalog_key("2026/10/19/09/member_00");
    meta.failing.lock().unwrap().insert(member.clone());
    let report = prober.poll(&store, &store, t0 + SETTLED_RECHECK).unwrap();
    assert_eq!((report.catalogs, report.catalogs_failed), (1, 1));
    assert!(report.unreachable(), "its only request failed");
    assert_eq!(prober.files().len(), 1, "kept as it was");
    meta.take_fetched();

    meta.failing.lock().unwrap().clear();
    let report = prober
        .poll(&store, &store, t0 + SETTLED_RECHECK * 2)
        .unwrap();
    assert_eq!(
        meta.take_fetched(),
        [catalog_key("2026/10/19/09"), member],
        "from the run directory down again"
    );
    assert_eq!(report.catalogs_failed, 0);
    assert_eq!(prober.files().len(), 1);
}

/// A catalog naming no HTTPServer service, or serving its files from
/// another origin than data_path's, fails its keys: they cannot be read by
/// range there.
#[tokio::test(flavor = "multi_thread")]
async fn files_must_be_served_where_data_path_reads() {
    let (meta, store) = store_to_the_day();
    put_run(&meta, 9, &[0], "2026-10-19T09:30:00Z");
    let member = catalog_key("2026/10/19/09/member_00");
    let files = [entry(
        &url_path(&sfc(9, 0)),
        "167.8",
        "Mbytes",
        "2026-10-19T09:30:00Z",
    )];
    let t0 = at("2026-10-19T10:00:00Z");
    meta.put_body(&member, catalog_xml(&[], &files, false));
    let mut dap_only = prober(sfc_spec(&[0], &[9], "-PT3H"));
    let report = dap_only.poll(&store, &store, t0).unwrap();
    assert_eq!((report.checked, report.heads), (1, 0));
    assert!(dap_only.files().is_empty());

    meta.put_body(&member, catalog_xml(&[], &files, true));
    let mut elsewhere = CatalogProber::new(
        sfc_spec(&[0], &[9], "-PT3H"),
        &catalog_url(),
        "https://files.test/thredds/fileServer/meps25epsarchive/",
    )
    .unwrap();
    elsewhere.poll(&store, &store, t0).unwrap();
    assert!(elsewhere.files().is_empty());

    let url = Url::parse(&[ORIGIN, member.as_str()].join("/")).unwrap();
    let key = file_key(&sfc(9, 0));
    for (catalog, why) in [
        (catalog_xml(&[], &files, false), "no HTTPServer service"),
        (catalog_xml(&[], &files, true), "not from data_path's"),
    ] {
        let answers = elsewhere
            .walk
            .answer(&parse(&catalog).unwrap(), &url, &[key.as_str()]);
        let error = answers[0].as_ref().unwrap_err().to_string();
        assert!(error.contains(why), "{error}");
    }
}

/// Through OPeNDAP (#1264), a catalog that serves its files by OPeNDAP only,
/// as thredds.met.no's MEPS catalogs have since 2026-10-22, lists them
/// under its `OpenDAP` base, each with the catalog's own version: nothing
/// is HEADed. A reader's recheck reads the catalog again; the same
/// catalog names no file to a prober reading by range.
#[tokio::test(flavor = "multi_thread")]
async fn opendap_files_take_the_catalogs_version_without_a_head() {
    const DAP: &str = "thredds/dodsC/meps25epsarchive";
    let (meta, store) = store_to_the_day();
    put_run(&meta, 9, &[0, 1], "2026-10-19T09:30:00Z");
    let member = catalog_key("2026/10/19/09/member_00");
    let files = [0, 1].map(|lead| {
        entry(
            &url_path(&sfc(9, lead)),
            "167.8",
            "Mbytes",
            "2026-10-19T09:30:00Z",
        )
    });
    meta.put_body(&member, catalog_xml(&[], &files, false));
    let spec = TemplateSpec {
        base: DAP.into(),
        ..sfc_spec(&[0, 1], &[9], "-PT3H")
    };
    let dap_url = [ORIGIN, DAP, ""].join("/");
    let mut prober = CatalogProber::new(spec, &catalog_url(), &dap_url)
        .unwrap()
        .with_service(FileService::OpenDap);
    let t0 = at("2026-10-19T10:00:00Z");
    meta.take_headed();
    // The sentinel, then the run's other lead.
    let reports = [0, 1].map(|_| prober.poll(&store, &store, t0).unwrap());
    assert_eq!(reports.each_ref().map(|r| r.heads), [0, 0]);
    assert!(meta.take_headed().is_empty());
    let listed = prober.files();
    let key = [DAP, sfc(9, 0).as_str()].join("/");
    assert_eq!(keys(&listed), [key.clone(), [DAP, &sfc(9, 1)].join("/")]);
    assert_eq!(
        listed[0].version,
        ObjectVersion {
            size: 167_800_000,
            last_modified: Some(at("2026-10-19T09:30:00Z")),
            e_tag: None,
        }
    );
    assert_eq!([&reports[0].added[..], &reports[1].added].concat(), listed);

    meta.take_fetched();
    assert!(prober.recheck(&key));
    let report = prober.poll(&store, &store, t0).unwrap();
    assert_eq!((report.catalogs, report.heads), (1, 0));
    assert_eq!(meta.take_fetched(), [member]);
    assert_eq!(prober.files(), listed);

    let spec = TemplateSpec {
        base: DAP.into(),
        ..sfc_spec(&[0, 1], &[9], "-PT3H")
    };
    let mut by_range = CatalogProber::new(spec, &catalog_url(), &dap_url).unwrap();
    by_range.poll(&store, &store, t0).unwrap();
    assert!(by_range.files().is_empty(), "no HTTPServer service");
}
