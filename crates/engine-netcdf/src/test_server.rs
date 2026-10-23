//! A local HTTP/1.1 server of fixed objects for the remote-read tests,
//! answering like thredds.met.no's fileServer: HEAD with `Content-Length`
//! and `Last-Modified` but no ETag, a ranged GET with 206 and
//! `Content-Range`, 412 for a GET whose `If-Unmodified-Since` the object is
//! newer than, 404 for anything else. One thread per connection, each
//! closed after its response. Every request is logged. Optionally a whole
//! GET is answered as thredds.met.no answers a catalog: chunked, without
//! `Content-Length` ([`TestServer::chunk_whole_gets`]). Also the THREDDS
//! catalogs such a server publishes ([`thredds_catalog`], #1170), written
//! with `quick_xml::Writer`, never `format!` (Critical Rule 3). And
//! OPeNDAP (DAP2) datasets, as thredds.met.no's `dodsC` answers for a
//! NetCDF file (#1264): `.dds`, `.das` and `.dods` with a constraint
//! expression, chunked, without `Last-Modified` unless asked
//! ([`TestServer::put_opendap`]). Shared by the lib's tests and
//! `tests/integration.rs` (`#[path]`).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

/// One request the server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    /// The requested `bytes=start-end`, inclusive.
    pub range: Option<(u64, u64)>,
    pub status: u16,
    /// Answered `Transfer-Encoding: chunked`, without `Content-Length`.
    pub chunked: bool,
}

#[derive(Default)]
struct State {
    objects: HashMap<String, (Arc<Vec<u8>>, DateTime<Utc>)>,
    /// OPeNDAP datasets by path, answered at `<path>.dds` and the rest.
    datasets: HashMap<String, Arc<Dataset>>,
    log: Vec<Request>,
    /// GETs of more than this many bytes wait this long before answering.
    slow: Option<(u64, Duration)>,
    /// Whole (unranged) GETs are answered chunked.
    chunked: bool,
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

    /// Serve the NetCDF file `bytes` as an OPeNDAP dataset at `/path`
    /// (`/path.dds`, `.das`, `.dods`), in place of what was there: as THREDDS
    /// does with `grids`, a variable whose every dimension has a coordinate
    /// variable a `Grid`, else every variable a plain array. Its replies
    /// carry `last_modified` when given; THREDDS sends none.
    pub fn put_opendap(
        &self,
        path: &str,
        bytes: Vec<u8>,
        grids: bool,
        last_modified: Option<DateTime<Utc>>,
    ) {
        let dataset = Dataset::new(bytes, grids, last_modified);
        self.state
            .lock()
            .unwrap()
            .datasets
            .insert(path.trim_start_matches('/').to_string(), Arc::new(dataset));
    }

    /// Stop serving the OPeNDAP dataset at `/path`: a 404 from then on.
    pub fn remove_opendap(&self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .datasets
            .remove(path.trim_start_matches('/'));
    }

    /// Answer every whole GET, a 200, chunked and without `Content-Length`,
    /// as thredds.met.no answers `catalog.xml` (#1170). Ranged GETs and
    /// HEADs keep their lengths.
    pub fn chunk_whole_gets(&self) {
        self.state.lock().unwrap().chunked = true;
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
    let (stem, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
    let dap = [".dds", ".das", ".dods"].iter().find_map(|suffix| {
        let dataset = stem.strip_suffix(suffix)?;
        let found = state.lock().unwrap().datasets.get(dataset).cloned()?;
        Some((*suffix, found))
    });
    if let Some((suffix, dataset)) = dap {
        let (status, response) = dataset.answer(suffix, query);
        state.lock().unwrap().log.push(Request {
            method,
            path,
            range: None,
            status,
            chunked: status == 200,
        });
        let _ = stream.write_all(&response);
        return;
    }
    let (object, slow, chunked) = {
        let state = state.lock().unwrap();
        (state.objects.get(&path).cloned(), state.slow, state.chunked)
    };
    let (status, response, chunked) = respond(&method, object, range, since, chunked);
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
        chunked,
    });
    let _ = stream.write_all(&response);
}

fn respond(
    method: &str,
    object: Option<(Arc<Vec<u8>>, DateTime<Utc>)>,
    range: Option<(u64, u64)>,
    since: Option<DateTime<chrono::FixedOffset>>,
    chunked: bool,
) -> (u16, Vec<u8>, bool) {
    let status = |code: u16, text: &str| {
        (
            code,
            format!("HTTP/1.1 {code} {text}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .into_bytes(),
            false,
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
    if chunked && code == 200 && method == "GET" {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/xml;charset=UTF-8\r\n\
             Transfer-Encoding: chunked\r\nLast-Modified: {date}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        for chunk in body.chunks(4096) {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        return (code, response, true);
    }
    let mut response = format!(
        "HTTP/1.1 {code} {text}\r\nContent-Length: {}\r\n{extra}Last-Modified: {date}\r\n\
         Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        span.end - span.start
    )
    .into_bytes();
    if method == "GET" {
        response.extend_from_slice(&body[span.start as usize..span.end as usize]);
    }
    (code, response, false)
}

/// `<name …/>`, or with `body` as its text.
fn element(w: &mut Writer<Vec<u8>>, name: &str, attributes: &[(&str, &str)], body: Option<&str>) {
    let start = BytesStart::new(name).with_attributes(attributes.iter().copied());
    match body {
        None => w.write_event(Event::Empty(start)).unwrap(),
        Some(body) => {
            w.write_event(Event::Start(start)).unwrap();
            w.write_event(Event::Text(BytesText::new(body))).unwrap();
            w.write_event(Event::End(BytesEnd::new(name))).unwrap();
        }
    }
}

/// A THREDDS catalog as thredds.met.no writes one: an OpenDAP and an
/// HTTPServer service, a `catalogRef` to each of `refs`, and `files` as
/// (urlPath, dataSize in Kbytes, modified).
pub(crate) fn thredds_catalog(refs: &[&str], files: &[(String, String, String)]) -> Vec<u8> {
    let mut w = Writer::new(Vec::new());
    let open = |w: &mut Writer<Vec<u8>>, name: &str, attributes: &[(&str, &str)]| {
        let start = BytesStart::new(name).with_attributes(attributes.iter().copied());
        w.write_event(Event::Start(start)).unwrap();
    };
    let close = |w: &mut Writer<Vec<u8>>, name: &str| {
        w.write_event(Event::End(BytesEnd::new(name))).unwrap();
    };
    open(
        &mut w,
        "catalog",
        &[
            (
                "xmlns",
                "http://www.unidata.ucar.edu/namespaces/thredds/InvCatalog/v1.0",
            ),
            ("xmlns:xlink", "http://www.w3.org/1999/xlink"),
        ],
    );
    open(
        &mut w,
        "service",
        &[("name", "all"), ("serviceType", "Compound"), ("base", "")],
    );
    for (kind, base) in [
        ("OpenDAP", "/thredds/dodsC/"),
        ("HTTPServer", "/thredds/fileServer/"),
    ] {
        element(
            &mut w,
            "service",
            &[("name", kind), ("serviceType", kind), ("base", base)],
            None,
        );
    }
    close(&mut w, "service");
    open(&mut w, "dataset", &[("name", "dir/")]);
    for (url_path, size, modified) in files {
        let name = url_path.rsplit('/').next().unwrap();
        open(
            &mut w,
            "dataset",
            &[("name", name), ("urlPath", url_path.as_str())],
        );
        element(&mut w, "dataSize", &[("units", "Kbytes")], Some(size));
        element(&mut w, "date", &[("type", "modified")], Some(modified));
        close(&mut w, "dataset");
    }
    for href in refs {
        let title = href.trim_end_matches("/catalog.xml");
        element(
            &mut w,
            "catalogRef",
            &[("xlink:href", href), ("xlink:title", title)],
            None,
        );
    }
    close(&mut w, "dataset");
    close(&mut w, "catalog");
    w.into_inner()
}

/// An OPeNDAP dataset: a NetCDF file, its DDS and DAS.
struct Dataset {
    nc: netcdf_reader::NcFile,
    variables: Vec<netcdf_reader::NcVariable>,
    /// The variables served as Grids.
    grids: HashSet<String>,
    dds: String,
    das: String,
    last_modified: Option<DateTime<Utc>>,
}

/// The DAP2 type of a netCDF type, if DAP2 has one.
fn dap_type(dtype: &netcdf_reader::NcType) -> Option<&'static str> {
    use netcdf_reader::NcType;
    Some(match dtype {
        NcType::Byte | NcType::UByte => "Byte",
        NcType::Short => "Int16",
        NcType::UShort => "UInt16",
        NcType::Int => "Int32",
        NcType::UInt => "UInt32",
        NcType::Float => "Float32",
        NcType::Double => "Float64",
        _ => return None,
    })
}

/// A quoted DAS string, `"` and `\` escaped.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// An attribute's DAS type and values.
fn das_values(value: &netcdf_reader::NcAttrValue) -> (&'static str, Vec<String>) {
    use netcdf_reader::NcAttrValue as V;
    fn all<T: std::fmt::Debug>(values: &[T]) -> Vec<String> {
        values.iter().map(|v| format!("{v:?}")).collect()
    }
    match value {
        V::Bytes(v) => ("Byte", all(v)),
        V::UBytes(v) => ("Byte", all(v)),
        V::Shorts(v) => ("Int16", all(v)),
        V::UShorts(v) => ("UInt16", all(v)),
        V::Ints(v) => ("Int32", all(v)),
        V::UInts(v) => ("UInt32", all(v)),
        V::Int64s(v) => ("Int64", all(v)),
        V::UInt64s(v) => ("UInt64", all(v)),
        V::Floats(v) => ("Float32", all(v)),
        V::Doubles(v) => ("Float64", all(v)),
        V::Chars(text) => ("String", vec![quoted(text)]),
        V::Strings(texts) => ("String", texts.iter().map(|t| quoted(t)).collect()),
    }
}

/// One hyperslab of a dimension: start, stride, inclusive stop.
type Slab = (u64, u64, u64);

impl Dataset {
    fn new(bytes: Vec<u8>, grids: bool, last_modified: Option<DateTime<Utc>>) -> Self {
        let storage: netcdf_reader::DynStorage = Arc::new(netcdf_reader::BytesStorage::new(bytes));
        let nc = netcdf_reader::NcFile::from_storage(storage).unwrap();
        let variables: Vec<netcdf_reader::NcVariable> = nc
            .variables()
            .unwrap()
            .iter()
            .filter(|v| dap_type(&v.dtype).is_some())
            .cloned()
            .collect();
        let coordinate = |dim: &str| {
            variables
                .iter()
                .find(|v| v.name == dim && v.is_coordinate_variable())
        };
        let grids: HashSet<String> = variables
            .iter()
            .filter(|v| {
                grids
                    && !v.dimensions.is_empty()
                    && !v.is_coordinate_variable()
                    && v.dimensions.iter().all(|d| coordinate(&d.name).is_some())
            })
            .map(|v| v.name.clone())
            .collect();
        let mut dds = String::from("Dataset {\n");
        for var in &variables {
            let whole: Vec<Slab> = var.dimensions.iter().map(|d| (0, 1, d.size - 1)).collect();
            dds.push_str(&Self::decl(
                &variables,
                var,
                &whole,
                grids.contains(&var.name),
            ));
        }
        dds.push_str("} test.nc;\n");
        let mut das = String::from("Attributes {\n");
        let mut unlimited = None;
        for var in &variables {
            das.push_str(&format!("    {} {{\n", var.name));
            for attribute in &var.attributes {
                let (ty, values) = das_values(&attribute.value);
                das.push_str(&format!(
                    "        {ty} {} {};\n",
                    attribute.name,
                    values.join(", ")
                ));
            }
            if var.dtype == netcdf_reader::NcType::Byte {
                das.push_str("        String _Unsigned \"false\";\n");
            }
            das.push_str("    }\n");
            unlimited = unlimited.or(var
                .dimensions
                .iter()
                .find(|d| d.is_unlimited)
                .map(|d| d.name.clone()));
        }
        if let Some(name) = unlimited {
            das.push_str(&format!(
                "    DODS_EXTRA {{\n        String Unlimited_Dimension {};\n    }}\n",
                quoted(&name)
            ));
        }
        das.push_str("}\n");
        Self {
            nc,
            variables,
            grids,
            dds,
            das,
            last_modified,
        }
    }

    /// `var`'s declaration over `slabs`: a Grid's with its maps, sliced
    /// alike, when `grid`.
    fn decl(
        variables: &[netcdf_reader::NcVariable],
        var: &netcdf_reader::NcVariable,
        slabs: &[Slab],
        grid: bool,
    ) -> String {
        let base = |v: &netcdf_reader::NcVariable, slabs: &[Slab]| {
            let dims: String = v
                .dimensions
                .iter()
                .zip(slabs)
                .map(|(d, (start, stride, stop))| {
                    format!("[{} = {}]", d.name, (stop - start) / stride + 1)
                })
                .collect();
            format!("{} {}{dims};\n", dap_type(&v.dtype).unwrap(), v.name)
        };
        if !grid {
            return format!("    {}", base(var, slabs));
        }
        let mut text = format!(
            "    Grid {{\n     ARRAY:\n        {}     MAPS:\n",
            base(var, slabs)
        );
        for (dim, slab) in var.dimensions.iter().zip(slabs) {
            let map = variables.iter().find(|v| v.name == dim.name).unwrap();
            text.push_str(&format!("        {}", base(map, &[*slab])));
        }
        text.push_str(&format!("    }} {};\n", var.name));
        text
    }

    /// The response to `<dataset><suffix>?<query>`.
    fn answer(&self, suffix: &str, query: &str) -> (u16, Vec<u8>) {
        let (kind, body) = match suffix {
            ".dds" => ("text/plain", self.dds.clone().into_bytes()),
            ".das" => ("text/plain", self.das.clone().into_bytes()),
            _ => match self.data(query) {
                Ok(body) => ("application/octet-stream", body),
                Err(message) => {
                    let body = format!(
                        "Error {{\n    code = 1005;\n    message = {};\n}};\n",
                        quoted(&message)
                    );
                    return (
                        400,
                        format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .into_bytes(),
                    );
                }
            },
        };
        let modified = self.last_modified.map_or(String::new(), |date| {
            format!(
                "Last-Modified: {}\r\n",
                date.format("%a, %d %b %Y %H:%M:%S GMT")
            )
        });
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nTransfer-Encoding: chunked\r\n\
             XDODS-Server: opendap/3.7\r\n{modified}Connection: close\r\n\r\n"
        )
        .into_bytes();
        for chunk in body.chunks(4096) {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        (200, response)
    }

    /// The `.dods` reply to a constraint: the projections' declarations in
    /// dataset order (a Grid's array projected as `g.g` inside a
    /// Structure, as THREDDS answers), `Data:`, then their values in XDR.
    fn data(&self, query: &str) -> Result<Vec<u8>, String> {
        let query = percent_decoded(query);
        let mut selected: Vec<(usize, bool, Vec<Slab>)> = Vec::new();
        for projection in query.split(',').filter(|p| !p.is_empty()) {
            let (name, slabs) = projection
                .split_once('[')
                .map_or((projection, ""), |(n, rest)| (n, rest));
            let (name, member) = match name.split_once('.') {
                Some((grid, member)) if grid == member && self.grids.contains(grid) => (grid, true),
                Some(_) => return Err(format!("no member '{name}'")),
                None => (name, false),
            };
            let index = self
                .variables
                .iter()
                .position(|v| v.name == name)
                .ok_or_else(|| format!("no variable '{name}'"))?;
            let var = &self.variables[index];
            let mut parsed = Vec::new();
            for slab in slabs.split('[').filter(|s| !s.is_empty()) {
                let slab = slab.trim_end_matches(']');
                let numbers: Vec<u64> = slab
                    .split(':')
                    .map(|n| n.parse().map_err(|_| format!("bad hyperslab '{slab}'")))
                    .collect::<Result<_, _>>()?;
                parsed.push(match numbers[..] {
                    [i] => (i, 1, i),
                    [start, stop] => (start, 1, stop),
                    [start, stride, stop] => (start, stride, stop),
                    _ => return Err(format!("bad hyperslab '{slab}'")),
                });
            }
            if parsed.is_empty() {
                parsed = var.dimensions.iter().map(|d| (0, 1, d.size - 1)).collect();
            }
            if parsed.len() != var.dimensions.len()
                || parsed
                    .iter()
                    .zip(&var.dimensions)
                    .any(|((start, stride, stop), d)| {
                        start > stop || *stop >= d.size || *stride == 0
                    })
            {
                return Err(format!("hyperslab out of '{name}'"));
            }
            selected.push((index, member, parsed));
        }
        selected.sort_by_key(|(index, ..)| *index);
        let mut dds = String::from("Dataset {\n");
        let mut data = Vec::new();
        for (index, member, slabs) in &selected {
            let var = &self.variables[*index];
            let grid = self.grids.contains(&var.name);
            if *member {
                dds.push_str(&format!(
                    "    Structure {{\n    {}    }} {};\n",
                    Self::decl(&self.variables, var, slabs, false),
                    var.name
                ));
            } else {
                dds.push_str(&Self::decl(&self.variables, var, slabs, grid));
            }
            self.encode(var, slabs, &mut data);
            if grid && !*member {
                for (dim, slab) in var.dimensions.iter().zip(slabs) {
                    let map = self.variables.iter().find(|v| v.name == dim.name).unwrap();
                    self.encode(map, &[*slab], &mut data);
                }
            }
        }
        dds.push_str("} test.nc;\n");
        let mut reply = dds.into_bytes();
        reply.extend_from_slice(b"\nData:\n");
        reply.extend_from_slice(&data);
        Ok(reply)
    }

    /// `var`'s values over `slabs` in XDR.
    fn encode(&self, var: &netcdf_reader::NcVariable, slabs: &[Slab], out: &mut Vec<u8>) {
        use netcdf_reader::{NcSliceInfo, NcSliceInfoElem, NcType};
        let values: Vec<f64> = if var.dimensions.is_empty() {
            self.nc
                .read_variable_as_f64(&var.name)
                .unwrap()
                .iter()
                .copied()
                .collect()
        } else {
            let selections = slabs
                .iter()
                .map(|&(start, step, stop)| NcSliceInfoElem::Slice {
                    start,
                    end: stop + 1,
                    step,
                })
                .collect();
            self.nc
                .read_variable_slice_as_f64(&var.name, &NcSliceInfo { selections })
                .unwrap()
                .iter()
                .copied()
                .collect()
        };
        let n = values.len() as u32;
        let array = !var.dimensions.is_empty();
        if array {
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
        }
        let word = |out: &mut Vec<u8>, w: u32| out.extend_from_slice(&w.to_be_bytes());
        match var.dtype {
            NcType::Byte | NcType::UByte if array => {
                out.extend(values.iter().map(|&v| v as i64 as u8));
                out.resize(out.len() + (n as usize).next_multiple_of(4) - n as usize, 0);
            }
            NcType::Byte | NcType::UByte => word(out, values[0] as i64 as u8 as u32),
            NcType::Short | NcType::Int => values.iter().for_each(|&v| word(out, v as i32 as u32)),
            NcType::UShort | NcType::UInt => values.iter().for_each(|&v| word(out, v as u32)),
            NcType::Float => values.iter().for_each(|&v| word(out, (v as f32).to_bits())),
            _ => values
                .iter()
                .for_each(|v| out.extend_from_slice(&v.to_be_bytes())),
        }
    }
}

/// `%XX` escapes decoded.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], text.get(i + 1..i + 3)) {
            (b'%', Some(hex)) if u8::from_str_radix(hex, 16).is_ok() => {
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
