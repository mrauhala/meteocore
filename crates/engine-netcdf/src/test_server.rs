//! A local HTTP/1.1 server of fixed objects for the remote-read tests,
//! answering like thredds.met.no's fileServer: HEAD with `Content-Length`
//! and `Last-Modified` but no ETag, a ranged GET with 206 and
//! `Content-Range`, 412 for a GET whose `If-Unmodified-Since` the object is
//! newer than, 404 for anything else. One thread per connection, each
//! closed after its response. Every request is logged.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

/// One request the server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    /// The requested `bytes=start-end`, inclusive.
    pub range: Option<(u64, u64)>,
    pub status: u16,
}

#[derive(Default)]
struct State {
    objects: HashMap<String, (Arc<Vec<u8>>, DateTime<Utc>)>,
    log: Vec<Request>,
    /// GETs of more than this many bytes wait this long before answering.
    slow: Option<(u64, Duration)>,
}

pub(crate) struct TestServer {
    pub url: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    pub fn new() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (serving, stopping) = (state.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                let state = serving.clone();
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self {
            url,
            state,
            stop,
            worker: Some(worker),
        }
    }

    /// Serve `body` at `/path`, modified at `modified`, in place of what was
    /// there.
    pub fn put(&self, path: &str, body: Vec<u8>, modified: DateTime<Utc>) {
        self.state.lock().unwrap().objects.insert(
            path.trim_start_matches('/').to_string(),
            (Arc::new(body), modified),
        );
    }

    /// Delay GETs of more than `bytes` by `delay`.
    pub fn slow_over(&self, bytes: u64, delay: Duration) {
        self.state.lock().unwrap().slow = Some((bytes, delay));
    }

    /// The requests answered since the last call.
    pub fn take(&self) -> Vec<Request> {
        std::mem::take(&mut self.state.lock().unwrap().log)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// GETs among `requests`.
pub(crate) fn gets(requests: &[Request]) -> Vec<&Request> {
    requests.iter().filter(|r| r.method == "GET").collect()
}

fn serve(mut stream: std::net::TcpStream, state: &Mutex<State>) {
    // macOS hands an accepted socket the listener's O_NONBLOCK.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut head = Vec::new();
    let mut buf = [0; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let mut words = head.split_whitespace();
    let (method, path) = (
        words.next().unwrap_or_default().to_string(),
        words
            .next()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string(),
    );
    let header = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    };
    let range = header("range")
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    let since = header("if-unmodified-since").and_then(|d| DateTime::parse_from_rfc2822(d).ok());
    let (object, slow) = {
        let state = state.lock().unwrap();
        (state.objects.get(&path).cloned(), state.slow)
    };
    let (status, response) = respond(&method, object, range, since);
    if let (Some((over, delay)), Some((start, end))) = (slow, range) {
        if method == "GET" && end + 1 - start > over && status == 206 {
            std::thread::sleep(delay);
        }
    }
    state.lock().unwrap().log.push(Request {
        method,
        path,
        range,
        status,
    });
    let _ = stream.write_all(&response);
}

fn respond(
    method: &str,
    object: Option<(Arc<Vec<u8>>, DateTime<Utc>)>,
    range: Option<(u64, u64)>,
    since: Option<DateTime<chrono::FixedOffset>>,
) -> (u16, Vec<u8>) {
    let status = |code: u16, text: &str| {
        (
            code,
            format!("HTTP/1.1 {code} {text}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .into_bytes(),
        )
    };
    let Some((body, modified)) = object else {
        return status(404, "Not Found");
    };
    if method == "GET" && since.is_some_and(|since| modified.timestamp() > since.timestamp()) {
        return status(412, "Precondition Failed");
    }
    let total = body.len() as u64;
    let date = modified.format("%a, %d %b %Y %H:%M:%S GMT");
    let (code, text, span, extra) = match range {
        Some((start, end)) if method == "GET" => {
            let end = end.min(total - 1);
            (
                206,
                "Partial Content",
                start..end + 1,
                format!("Content-Range: bytes {start}-{end}/{total}\r\n"),
            )
        }
        _ => (200, "OK", 0..total, String::new()),
    };
    let mut response = format!(
        "HTTP/1.1 {code} {text}\r\nContent-Length: {}\r\n{extra}Last-Modified: {date}\r\n\
         Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        span.end - span.start
    )
    .into_bytes();
    if method == "GET" {
        response.extend_from_slice(&body[span.start as usize..span.end as usize]);
    }
    (code, response)
}
