//! A loopback object server whose requests are answered explicitly by each test.
use super::*;
use ds_storage::object_store::{http::HttpBuilder, ClientOptions, RetryConfig};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{atomic::AtomicUsize, mpsc};
use std::thread::JoinHandle;

pub(super) struct HttpStore {
    pub(super) store: Arc<DsStore>,
    requests: mpsc::Receiver<Request>,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

pub(super) struct Request {
    path: String,
    socket: TcpStream,
}

impl Request {
    pub(super) fn reply(mut self, status: u16, body: &[u8]) {
        write!(
            self.socket,
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        self.socket.write_all(body).unwrap();
    }
}

impl HttpStore {
    pub(super) fn new(cache_mb: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let backend = HttpBuilder::new()
            .with_url(format!("http://{}", listener.local_addr().unwrap()))
            .with_client_options(ClientOptions::new().with_allow_http(true))
            .with_retry(RetryConfig {
                max_retries: 0,
                ..Default::default()
            })
            .build()
            .unwrap();
        let (send, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let server = {
            let stop = stop.clone();
            let calls = calls.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let mut socket = match listener.accept() {
                        Ok((socket, _)) => socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        assert!(headers.len() < 16 * 1024, "oversized test request");
                        let mut byte = [0];
                        socket.read_exact(&mut byte).unwrap();
                        headers.push(byte[0]);
                    }
                    let headers = String::from_utf8(headers).unwrap();
                    let mut request = headers.lines().next().unwrap().split_whitespace();
                    assert_eq!(request.next(), Some("GET"));
                    let path = request.next().unwrap().to_string();
                    calls.fetch_add(1, Ordering::Relaxed);
                    if send.send(Request { path, socket }).is_err() {
                        break;
                    }
                }
            })
        };
        Self {
            store: Arc::new(DsStore::new(
                DataStore::new(Arc::new(backend)),
                "",
                cache_mb,
            )),
            requests,
            calls,
            stop,
            server: Some(server),
        }
    }

    pub(super) fn next(&self, path: &str) -> Request {
        let request = self.requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(request.path, path);
        request
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    pub(super) fn assert_idle(&self) {
        assert!(
            matches!(
                self.requests.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "unexpected duplicate backend request"
        );
    }
}

impl Drop for HttpStore {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Do not mask an assertion failure with a second panic during cleanup.
        let _ = self.server.take().unwrap().join();
    }
}
