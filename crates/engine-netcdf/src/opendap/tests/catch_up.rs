//! A collection catching up after an upstream outage while its host
//! throttles (#1290): the catalog comes back listing runs newer than the
//! ones served, and the host answers their opens 429 Too Many Requests.
//! The served runs stay served until a newer run can serve data.

use super::*;

/// Four run slots three hours apart, the newest an hour before now on the
/// hour.
fn four_slots() -> [DateTime<Utc>; 4] {
    let (_, late) = slots();
    let step = chrono::Duration::hours(3);
    [late - step * 3, late - step * 2, late - step, late]
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// The MEPS sfc fixture's lead 00 of its `run` (`"06"` or `"09"`), the run
/// moved `hours` later: its `forecast_reference_time` and its one valid
/// time, the same instant at lead 00.
fn lead_00(run: &str, hours: i64) -> Vec<u8> {
    let fixture = ["meps-netcdf/meps_sfc_00_20261015T", run, "Z.nc"].concat();
    let mut bytes = std::fs::read(testdata(&fixture)).unwrap();
    let from = at(&["2026-10-15T", run, ":00:00Z"].concat());
    let to = from + chrono::Duration::hours(hours);
    let (from, to) = (
        (from.timestamp() as f64).to_le_bytes(),
        (to.timestamp() as f64).to_le_bytes(),
    );
    let stored: Vec<usize> = (0..bytes.len() - 8)
        .filter(|&i| bytes[i..i + 8] == from)
        .collect();
    assert_eq!(stored.len(), 2, "the run and the valid time");
    for i in stored {
        bytes[i..i + 8].copy_from_slice(&to);
    }
    bytes
}

/// Serve each of `runs`, `(file, slot)`, as lead 00 of its slot, and the
/// catalogs listing them all.
fn publish_runs(server: &TestServer, runs: &[(Vec<u8>, DateTime<Utc>)]) {
    let mut listed = Vec::new();
    for (bytes, slot) in runs {
        let (dir, name) = place("sfc", 0, *slot);
        listed.push((dir, name, bytes.len()));
        server.put_opendap(&dataset("sfc", 0, *slot), bytes.clone(), true, None);
    }
    catalogs(server, &listed);
}

/// The OPeNDAP directory of `slot`'s files, under which the server can be
/// told to refuse every request.
fn slot_dir(slot: DateTime<Utc>) -> String {
    let (dir, _) = place("sfc", 0, slot);
    [DAP, &dir].join("/")
}

/// A collection of lead 00 of every run in [`four_slots`], two runs kept.
fn catch_up_config(server: &TestServer) -> NetcdfConfig {
    let mut hours: Vec<u32> = four_slots().iter().map(|slot| slot.hour()).collect();
    hours.sort_unstable();
    toml::from_str(&format!(
        r#"access = "opendap"
           data_path = "{url}/{DAP}/"
           catalog_url = "{url}/{CATALOGS}/catalog.xml"
           prefix_pattern = "%Y/%m/%d/{{run}}/member_00/"
           filename_template = "meps_sfc_{{lead}}_%Y%m%dT{{run}}Z.nc"
           run_hours = {hours:?}
           leads = [0]
           time_window = "-PT12H"
           max_runs = 2
           variables = ["air_temperature_2m"]"#,
        url = server.url,
    ))
    .unwrap()
}

/// The runs `engine` serves, oldest first.
fn runs(engine: &NetcdfEngine) -> Vec<DateTime<Utc>> {
    engine.raster_info().reference_times
}

/// The issue's test: the catalog lists two runs newer than the two served,
/// and the host answers every request for their files 429. The served
/// runs stay served, so `collection_has_data` stays 1, until a newer run
/// opens: then it serves with the newer of the old ones, and the oldest is
/// closed; when both have opened, they are what is served.
#[test]
fn served_runs_stay_until_a_newer_run_serves() {
    let server = TestServer::new();
    let _quick = ds_storage::test_util::quick_backoff(&server.url);
    let [s0, s1, s2, s3] = four_slots();
    let old = vec![(lead_00("06", 0), s0), (lead_00("09", 0), s1)];
    publish_runs(&server, &old);
    let engine = opened(&catch_up_config(&server));
    let (six, nine) = (at("2026-10-15T06:00:00Z"), at("2026-10-15T09:00:00Z"));
    let (twelve, fifteen) = (at("2026-10-15T12:00:00Z"), at("2026-10-15T15:00:00Z"));
    assert_eq!(runs(&engine), [six, nine]);

    server.throttle(&slot_dir(s2));
    server.throttle(&slot_dir(s3));
    let new = vec![(lead_00("06", 6), s2), (lead_00("09", 6), s3)];
    publish_runs(&server, &[old, new].concat());
    server.take();
    engine.poll_once();
    let refused: BTreeSet<String> = server
        .take()
        .into_iter()
        .filter(|r| r.status == 429)
        .map(|r| r.path)
        .collect();
    assert!(
        refused.iter().any(|path| path.starts_with(&slot_dir(s2)))
            && refused.iter().any(|path| path.starts_with(&slot_dir(s3))),
        "both newer runs' opens were tried: {refused:#?}"
    );
    assert_eq!(runs(&engine), [six, nine], "the served runs stay served");
    assert!(engine.data_age().is_some(), "collection_has_data stays 1");

    // The newest run opens; the other newer one is still refused.
    server.unthrottle();
    server.throttle(&slot_dir(s2));
    engine.poll_once();
    assert_eq!(runs(&engine), [nine, fifteen]);

    server.unthrottle();
    engine.poll_once();
    assert_eq!(runs(&engine), [twelve, fifteen]);
}

/// The WARN lines the engine logs on the calling thread while `scan`
/// runs, where a scan logs its one line: not ds-storage's. One global
/// subscriber writes each thread's lines to that thread's buffer: a
/// subscriber of this thread alone (`with_default`) misses events whose
/// callsite another test's thread registered first.
fn warnings(scan: impl FnOnce()) -> Vec<String> {
    thread_local! {
        static LINES: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    struct ThreadLines;
    impl std::io::Write for ThreadLines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            LINES.with(|lines| lines.borrow_mut().extend_from_slice(bytes));
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    static GLOBAL: std::sync::Once = std::sync::Once::new();
    GLOBAL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(|| ThreadLines)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
    LINES.with(|lines| lines.borrow_mut().clear());
    scan();
    let bytes = LINES.with(|lines| std::mem::take(&mut *lines.borrow_mut()));
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| line.contains("engine_netcdf"))
        .map(String::from)
        .collect()
}

/// The files `requests` asked the DAP server for, by their dataset path.
fn datasets_asked(requests: &[Request]) -> BTreeSet<String> {
    dap_requests(requests)
        .into_iter()
        .map(|path| {
            let path = path.split_once('?').map_or(path.as_str(), |(p, _)| p);
            let stem = [".dds", ".das", ".dods"]
                .iter()
                .find_map(|suffix| path.strip_suffix(suffix));
            stem.unwrap_or(path).to_string()
        })
        .collect()
}

/// While its host refuses every open with 429, a scan opens the newest
/// run first and starts no more opens once [`crate::MAX_THROTTLED_OPENS`]
/// have failed so: the older run is not asked for at all, and is left for
/// the next poll. The scan logs one line for it all, not one per file.
/// Once the host answers again, a scan opens both runs.
#[test]
fn a_throttled_scan_stops_after_a_few_failed_opens() {
    let server = TestServer::new();
    let _quick = ds_storage::test_util::quick_backoff(&server.url);
    let engine = NetcdfEngine::new(
        "throttled",
        &config(
            &server,
            "sfc",
            &[0, 1, 2],
            r#"variables = ["air_temperature_2m"]"#,
        ),
    )
    .unwrap();
    assert!(engine.data_age().is_none(), "nothing published yet");
    let files = sfc_files();
    publish(&server, &files, true, None);
    server.throttle(DAP);
    server.take();
    let one_at_a_time = crate::Limits {
        opens: crate::MAX_OPENS_PER_POLL,
        concurrency: 1,
        until: None,
    };

    let warned = warnings(|| {
        engine
            .scan(&one_at_a_time, crate::INITIAL_PROBE_ROUNDS)
            .unwrap()
    });
    let (_, late) = slots();
    let newest: BTreeSet<String> = (0..3).map(|lead| dataset("sfc", lead, late)).collect();
    let asked = datasets_asked(&server.take());
    let asked: BTreeSet<String> = asked.into_iter().map(|d| [DAP, &d].join("/")).collect();
    assert_eq!(asked, newest, "the newest run's three files, no more");
    assert_eq!(warned.len(), 1, "one line for the scan: {warned:#?}");
    assert!(
        warned[0].contains(
            "opened 0 files this scan, 6 left for the next: 3 could not be read now, 3 \
             while the host answered 429 Too Many Requests, so the scan opened no more"
        ),
        "{}",
        warned[0]
    );
    assert!(engine.data_age().is_none());

    server.unthrottle();
    let warned = warnings(|| {
        engine
            .scan(&one_at_a_time, crate::INITIAL_PROBE_ROUNDS)
            .unwrap()
    });
    assert!(warned.is_empty(), "{warned:#?}");
    assert_eq!(runs(&engine).len(), 2);
}
