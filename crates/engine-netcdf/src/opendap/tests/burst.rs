//! The requests a restart, a run's arrival and a catch-up after an outage
//! send a host that answers 429 Too Many Requests over a rate (#1290),
//! measured. Ignored: it runs for minutes in real time and prints what it
//! found, for the numbers of a change to the back-off:
//! `cargo test -p engine-netcdf --lib burst -- --ignored --nocapture`.

use std::time::{Duration, Instant};

use super::*;

/// Leads per run, as MEPS publishes them.
const LEADS: u32 = 67;

/// A host that serves a request in 40 ms and answers 429, at once and
/// without `Retry-After`, to requests over 10 a second after a burst of 20.
fn host() -> TestServer {
    let server = TestServer::new();
    server.latency(Duration::from_millis(40));
    server.rate_limit(10.0, 20.0);
    server
}

/// The MEPS sfc fixture's lead 00 of 09Z, its run and valid time moved
/// `hours` later.
fn run_file(hours: i64) -> Vec<u8> {
    let mut bytes = std::fs::read(testdata("meps-netcdf/meps_sfc_00_20261015T09Z.nc")).unwrap();
    let from: DateTime<Utc> = "2026-10-15T09:00:00Z".parse().unwrap();
    let to = from + chrono::Duration::hours(hours);
    let (from, to) = (
        (from.timestamp() as f64).to_le_bytes(),
        (to.timestamp() as f64).to_le_bytes(),
    );
    for i in 0..bytes.len() - 8 {
        if bytes[i..i + 8] == from {
            bytes[i..i + 8].copy_from_slice(&to);
        }
    }
    bytes
}

/// Serve `runs`, `(file, slot)`, each as [`LEADS`] leads of one file, with
/// the catalogs listing them all.
fn publish_runs(server: &TestServer, runs: &[(Vec<u8>, DateTime<Utc>)]) {
    let mut listed = Vec::new();
    for (bytes, slot) in runs {
        for lead in 0..LEADS {
            let (dir, name) = place("sfc", lead, *slot);
            listed.push((dir, name, bytes.len()));
            server.put_opendap(&dataset("sfc", lead, *slot), bytes.clone(), true, None);
        }
    }
    catalogs(server, &listed);
}

/// Four slots three hours apart, the newest an hour ago.
fn four() -> [DateTime<Utc>; 4] {
    let (_, late) = slots();
    let step = chrono::Duration::hours(3);
    [late - step * 3, late - step * 2, late - step, late]
}

fn burst_config(server: &TestServer) -> NetcdfConfig {
    use chrono::Timelike;
    let mut hours: Vec<u32> = four().iter().map(|slot| slot.hour()).collect();
    hours.sort_unstable();
    let leads: Vec<u32> = (0..LEADS).collect();
    toml::from_str(&format!(
        r#"access = "opendap"
           data_path = "{url}/{DAP}/"
           catalog_url = "{url}/{CATALOGS}/catalog.xml"
           prefix_pattern = "%Y/%m/%d/{{run}}/member_00/"
           filename_template = "meps_sfc_{{lead}}_%Y%m%dT{{run}}Z.nc"
           run_hours = {hours:?}
           leads = {leads:?}
           time_window = "-PT12H"
           max_runs = 2
           variables = ["air_temperature_2m"]"#,
        url = server.url,
    ))
    .unwrap()
}

/// What a scenario cost the host and the collection.
#[derive(Debug)]
struct Cost {
    started: Instant,
    /// Polls until every file was open, a first scan counted.
    polls: usize,
    requests: usize,
    /// Of them answered 429.
    refused: usize,
    /// Files opened by the first poll.
    first_poll: usize,
    /// Polls after which the collection served nothing.
    empty: usize,
    /// Files opened: their datasets' 200 `.dods`, an open's coordinates.
    opened: BTreeSet<String>,
}

impl Cost {
    /// Count from now, the server's log so far left out.
    fn start(server: &TestServer) -> Self {
        server.take();
        Cost {
            started: Instant::now(),
            polls: 0,
            requests: 0,
            refused: 0,
            first_poll: 0,
            empty: 0,
            opened: BTreeSet::new(),
        }
    }

    /// Count a scan or poll that has just run.
    fn polled(&mut self, server: &TestServer, engine: &NetcdfEngine) {
        let requests = server.take();
        self.polls += 1;
        self.requests += requests.len();
        self.refused += requests.iter().filter(|r| r.status == 429).count();
        for request in requests {
            if let (200, Some((dataset, _))) = (request.status, request.path.split_once(".dods")) {
                self.opened.insert(dataset.to_string());
            }
        }
        if self.polls == 1 {
            self.first_poll = self.opened.len();
        }
        self.empty += usize::from(engine.data_age().is_none());
    }

    /// Poll `engine` until `files` files have opened, at most 20 polls.
    fn until(mut self, server: &TestServer, engine: &NetcdfEngine, files: usize) -> Self {
        while self.opened.len() < files && self.polls < 20 {
            engine.poll_once();
            self.polled(server, engine);
        }
        assert_eq!(self.opened.len(), files, "{self:?}");
        self
    }

    fn row(&self, name: &str) -> String {
        format!(
            "{name} | {} | {} | {} | {} | {} | {:.0}",
            self.polls,
            self.requests,
            self.refused,
            self.first_poll,
            self.empty,
            self.started.elapsed().as_secs_f64()
        )
    }
}

/// An engine over `server` that has opened its one published run.
fn serving(server: &TestServer) -> NetcdfEngine {
    let cost = Cost::start(server);
    let engine = NetcdfEngine::new("burst", &burst_config(server)).unwrap();
    let mut cost = cost;
    cost.polled(server, &engine);
    cost.until(server, &engine, LEADS as usize);
    engine
}

#[test]
#[ignore = "minutes of real time; prints the bursts' numbers"]
fn burst() {
    let [s0, _, s2, s3] = four();
    let leads = LEADS as usize;
    let mut rows = Vec::new();

    // A restart: one run's 67 files, from the first scan on.
    let server = host();
    publish_runs(&server, &[(run_file(0), s3)]);
    let mut cost = Cost::start(&server);
    let engine = NetcdfEngine::new("burst", &burst_config(&server)).unwrap();
    cost.polled(&server, &engine);
    rows.push(cost.until(&server, &engine, leads).row("restart, 67 files"));

    // A run's arrival: one run served, the next published.
    let server = host();
    publish_runs(&server, &[(run_file(0), s2)]);
    let engine = serving(&server);
    publish_runs(&server, &[(run_file(0), s2), (run_file(3), s3)]);
    let mut cost = Cost::start(&server);
    engine.poll_once();
    cost.polled(&server, &engine);
    rows.push(
        cost.until(&server, &engine, leads)
            .row("run arrival, 67 files"),
    );

    // A catch-up after an outage: one run served, two newer at once.
    let server = host();
    publish_runs(&server, &[(run_file(0), s0)]);
    let engine = serving(&server);
    let all = [(run_file(0), s0), (run_file(6), s2), (run_file(9), s3)];
    publish_runs(&server, &all);
    let mut cost = Cost::start(&server);
    engine.poll_once();
    cost.polled(&server, &engine);
    rows.push(
        cost.until(&server, &engine, 2 * leads)
            .row("catch-up, 134 files"),
    );

    eprintln!(
        "scenario | polls | requests | 429s | opened by the first poll | polls serving nothing | seconds"
    );
    for row in rows {
        eprintln!("{row}");
    }
}
