use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn new(redirect_target: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let received = requests.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                }
                received.fetch_add(1, Ordering::Relaxed);
                let request = String::from_utf8(request).unwrap();
                let mut fields = request.split_whitespace();
                let method = fields.next().unwrap();
                let path = fields.next().unwrap();
                let response = if let Some(code) = path.strip_prefix("/redirect") {
                    format!("HTTP/1.1 {code} Redirect\r\nLocation: {redirect_target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                } else if request.to_ascii_lowercase().contains("range: bytes=1-2") {
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-2/4\r\nContent-Length: 2\r\nConnection: close\r\n\r\nbc".into()
                } else {
                    let body = if method == "HEAD" { "" } else { "abcd" };
                    format!("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nLast-Modified: Wed, 16 Sep 2026 00:00:00 GMT\r\nConnection: close\r\n\r\n{body}")
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        Self {
            url,
            stop,
            requests,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http_get_head_and_range_work_but_never_contact_redirect_targets() {
    // A second origin makes redirect following observable without relying on
    // the particular error text produced by reqwest/object_store.
    let target = Server::new(String::new());
    let server = Server::new(format!("{}/private", target.url));
    let (store, path) = ds_storage::build_store(&format!("{}/ok", server.url)).unwrap();
    assert_eq!(&store.get(&path).unwrap()[..], b"abcd");
    assert_eq!(store.head(&path).unwrap().size, 4);
    assert_eq!(&store.get_range(&path, 1..3).unwrap()[..], b"bc");

    for code in [301, 302, 303, 307, 308] {
        let (_, path) = ds_storage::build_store(&format!("{}/redirect{code}", server.url)).unwrap();
        assert!(store.get(&path).is_err(), "GET {code}");
        assert!(store.head(&path).is_err(), "HEAD {code}");
        assert!(store.get_range(&path, 1..3).is_err(), "range {code}");
    }
    assert_eq!(
        target.requests.load(Ordering::Relaxed),
        0,
        "no redirected request may reach the second origin"
    );
}
