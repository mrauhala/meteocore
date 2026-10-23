//! Discovery from THREDDS catalogs (#1170).
//!
//! A THREDDS Data Server publishes a `catalog.xml` for each directory: each
//! file (`dataset`) with its `urlPath`, size and modification time, each
//! subdirectory as a `catalogRef`, and the services the files are reached
//! through, the `HTTPServer` (fileServer) among them. Where a file server
//! cannot be listed, as thredds.met.no's fileServer cannot, its catalogs
//! are the listing.
//!
//! A [`CatalogProber`] is a [`TemplateProber`] whose checks a catalog
//! answers instead of a HEAD each ([`TemplateProber::poll_with`]). The
//! template still names the files: the prefix and filename templates, run
//! hours and leads. The catalog of each planned key's directory says
//! whether the file is there, and at which size and modification time.
//! Settling and publication are the template prober's: the shared
//! [`PendingFile`](super::PendingFile) rule over the catalog's version,
//! `min_age`, whole runs or `run_grace`, and the rechecks.
//!
//! # Walking to a directory
//!
//! Directories are found from the root catalog through `catalogRef`s, as a
//! client browses them. A ref is followed only when it leads toward the
//! directory of a key the poll checks, so a poll reads the catalogs on the
//! paths to the runs in the time window and nothing else, at most
//! [`MAX_CATALOGS_PER_POLL`]: never the whole server. Refs to another
//! origin, or outside the root catalog's directory, are never followed. A
//! directory's catalog URL, once found, is kept while a slot of the window
//! lies under it, so later polls read it directly: one GET per directory
//! checked. The catalog tree is taken to mirror the file tree under the
//! template's base, as THREDDS's `datasetScan` serves the catalog of
//! `<root>/D/` for the files of `<data_path>/D/`.
//!
//! Catalogs are fetched with [`DataStore::get_documents`], four at a time
//! and one storage-bridge call per level of the walk (Critical Rule 9),
//! each under [`FetchBudget::Request`] and the scan's deadline, through the
//! store the caller passes: an engine's host share, so its request limit
//! and 429 back-off apply. THREDDS streams catalogs chunked, without the
//! `Content-Length` object_store's GET needs, which `get_documents` reads
//! all the same, failing a catalog at [`MAX_CATALOG_BYTES`] while it
//! streams.
//!
//! # Services and versions
//!
//! The files are read through one of the catalog's services
//! ([`FileService`]): its `HTTPServer` by range (the default), or its
//! `OpenDAP` (#1264: thredds.met.no serves MEPS through OPeNDAP only since
//! 2026-10-22). A dataset's key is that service's `base` and its `urlPath`.
//!
//! A catalog states a file's size with four significant digits
//! (`71.56 Gbytes`), never its byte count. Its version, that size and the
//! `date type="modified"`, settles files, but it cannot be the identity
//! every conditional range read checks
//! ([`crate::DataStore::get_range_if_unmodified`] compares sizes exactly).
//! Read by range, a file is HEADed once when it is listed, and again only
//! when its catalog version changes or a reader asks
//! ([`CatalogProber::recheck`]), and [`CatalogProber::files`] carries that
//! HEAD's version. Read through OPeNDAP, nothing is HEADed: no read
//! compares sizes, and an OPeNDAP dataset URL is no file to HEAD, so a
//! file's version is the catalog's.
//!
//! # Parsing
//!
//! [`parse`] reads quick-xml events. Since quick-xml 0.38 an entity
//! reference in text (`&amp;`, `&#58;`) arrives as an `Event::GeneralRef`
//! of its own, so the text of `dataSize` and `date` is assembled from both;
//! attribute values are unescaped whole. Nothing here writes XML.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use ds_core::error::DataServerError;
use object_store::path::Path as ObjectPath;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use url::Url;

use super::template::{Answers, ProbeReport, ProbedFile, TemplateProber, TemplateSpec};
use super::PROBE_CONCURRENCY;
use crate::{DataStore, FetchBudget, ObjectVersion};

/// Most catalogs one [`CatalogProber::poll`] reads. A window's walk reads
/// a few per run directory (MEPS: about six from the archive root, then one
/// per directory); the rest wait for the next poll.
pub const MAX_CATALOGS_PER_POLL: usize = 64;

/// A catalog document larger than this is refused, as soon as its body
/// passes it. A MEPS day directory's is 21 KB.
pub const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

/// Catalog GETs in flight at once, within the store's own request limit.
const CATALOG_CONCURRENCY: usize = 4;

/// A parsed `catalog.xml`: what discovery reads of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Every `service`, the ones nested in a `Compound` service included.
    pub services: Vec<Service>,
    /// Every `dataset` with a `urlPath`: the files. Container datasets
    /// without one are not listed; their children are.
    pub datasets: Vec<Dataset>,
    pub refs: Vec<CatalogRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    /// `serviceType`, such as `HTTPServer`, `OpenDAP` or `Compound`.
    pub service_type: String,
    /// The URL a file's `urlPath` is appended to, usually an absolute path
    /// such as `/thredds/fileServer/`.
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dataset {
    pub name: String,
    /// The file's path under a service's `base`.
    pub url_path: String,
    /// `dataSize` in bytes, from its value and `units`, rounded as the
    /// catalog rounds it.
    pub size: Option<u64>,
    /// `date type="modified"`.
    pub modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRef {
    /// `xlink:href`, relative to the catalog that names it.
    pub href: String,
    /// `xlink:title`, else `name`.
    pub title: Option<String>,
}

impl Catalog {
    /// The `base` of the first `HTTPServer` service: the file server the
    /// files are read from by range.
    pub fn file_server_base(&self) -> Option<&str> {
        self.service_base(FileService::HttpServer)
    }

    /// The `base` of the first service of `service`'s type.
    pub fn service_base(&self, service: FileService) -> Option<&str> {
        self.services
            .iter()
            .find(|found| {
                found
                    .service_type
                    .eq_ignore_ascii_case(service.service_type())
            })
            .map(|found| found.base.as_str())
    }
}

/// The catalog service the files are read through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileService {
    /// `HTTPServer`, read by range: each listed file is HEADed for its
    /// exact version.
    #[default]
    HttpServer,
    /// `OpenDAP` (DAP2): a file's version is the catalog's, its rounded
    /// size and modification time, and nothing is HEADed.
    OpenDap,
}

impl FileService {
    /// The service's `serviceType` in a catalog.
    pub fn service_type(self) -> &'static str {
        match self {
            FileService::HttpServer => "HTTPServer",
            FileService::OpenDap => "OpenDAP",
        }
    }
}

/// Bytes of a `dataSize`, from its text and `units`. THREDDS writes `bytes`,
/// `Kbytes`, `Mbytes`, `Gbytes` and `Tbytes` with powers of 1000 (and reads
/// `Pbytes` too); the case of the unit does not matter. `None` for another
/// unit, or a value that is not a finite, non-negative number.
pub fn data_size(value: &str, units: &str) -> Option<u64> {
    let number: f64 = value.trim().parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    let scale = match units.trim().to_ascii_lowercase().as_str() {
        "" | "byte" | "bytes" => 1e0,
        "kbytes" => 1e3,
        "mbytes" => 1e6,
        "gbytes" => 1e9,
        "tbytes" => 1e12,
        "pbytes" => 1e15,
        _ => return None,
    };
    let bytes = (number * scale).round();
    (bytes < u64::MAX as f64).then_some(bytes as u64)
}

/// A catalog's `date`: RFC 3339 (`2026-10-19T04:05:37Z`), the same without
/// a zone (taken as UTC), or a bare date.
fn parse_date(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if let Ok(date) = DateTime::parse_from_rfc3339(text) {
        return Some(date.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(date.and_utc());
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|day| day.and_utc())
}

/// The text a general entity reference stands for: a character reference,
/// or one of XML's five predefined entities. Any other entity, which only a
/// DTD could define, is kept as written.
fn push_general_ref(text: &mut String, reference: &BytesRef) {
    if let Ok(Some(c)) = reference.resolve_char_ref() {
        text.push(c);
        return;
    }
    let name: &str = reference.as_ref();
    match name {
        "amp" => text.push('&'),
        "lt" => text.push('<'),
        "gt" => text.push('>'),
        "apos" => text.push('\''),
        "quot" => text.push('"'),
        other => {
            text.push('&');
            text.push_str(other);
            text.push(';');
        }
    }
}

/// The unescaped value of the attribute whose local name is `name`, so
/// `xlink:href` is `href`.
fn attribute(element: &BytesStart, name: &str) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|attr| attr.key.local_name().as_ref() == name)
        .and_then(|attr| {
            attr.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|value| value.into_owned())
        })
}

/// Which element's text is being read.
enum Field {
    Size { units: String },
    Modified,
}

/// A `dataset` being read: its children's `dataSize` and `date` land in the
/// innermost one.
struct Open {
    name: String,
    url_path: Option<String>,
    size: Option<u64>,
    modified: Option<DateTime<Utc>>,
}

/// Parse a THREDDS `catalog.xml` (InvCatalog 1.0 and later): its services,
/// file datasets and `catalogRef`s. Elements are matched by local name, so
/// any namespace prefix works. A document whose root is not `catalog` (an
/// HTML error page), malformed XML, or one over [`MAX_CATALOG_BYTES`] is an
/// error.
pub fn parse(xml: &[u8]) -> Result<Catalog, DataServerError> {
    let invalid = |why: String| DataServerError::Storage(format!("not a THREDDS catalog: {why}"));
    if xml.len() > MAX_CATALOG_BYTES {
        return Err(invalid(format!(
            "{} bytes, over the {MAX_CATALOG_BYTES}-byte limit",
            xml.len()
        )));
    }
    let xml = std::str::from_utf8(xml).map_err(|e| invalid(format!("not UTF-8: {e}")))?;
    let mut reader = Reader::from_str(xml);
    let mut catalog = Catalog::default();
    let mut root: Option<String> = None;
    let mut open: Vec<Open> = Vec::new();
    let mut field: Option<Field> = None;
    let mut text = String::new();
    loop {
        let event = reader
            .read_event()
            .map_err(|e| invalid(format!("at byte {}: {e}", reader.error_position())))?;
        let (element, empty) = match &event {
            Event::Start(element) => (Some(element), false),
            Event::Empty(element) => (Some(element), true),
            Event::Text(t) => {
                if field.is_some() {
                    text.push_str(&t.xml10_content());
                }
                continue;
            }
            // Since quick-xml 0.38: `&amp;` and `&#58;` are events of their
            // own, which a loop reading only `Text` silently drops.
            Event::GeneralRef(reference) => {
                if field.is_some() {
                    push_general_ref(&mut text, reference);
                }
                continue;
            }
            Event::CData(cdata) => {
                if field.is_some() {
                    text.push_str(cdata.as_ref());
                }
                continue;
            }
            Event::End(end) => {
                let name = end.local_name();
                match name.as_ref() {
                    "dataSize" | "date" => {
                        if let (Some(field), Some(dataset)) = (field.take(), open.last_mut()) {
                            match field {
                                Field::Size { units } => dataset.size = data_size(&text, &units),
                                Field::Modified => dataset.modified = parse_date(&text),
                            }
                        }
                    }
                    "dataset" => {
                        if let Some(dataset) = open.pop() {
                            finish(&mut catalog, dataset);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };
        let Some(element) = element else { continue };
        let name = element.local_name();
        let name: &str = name.as_ref();
        if root.is_none() {
            if name != "catalog" {
                return Err(invalid(format!("the root element is <{name}>")));
            }
            root = Some(name.to_string());
            continue;
        }
        match name {
            "service" => catalog.services.push(Service {
                name: attribute(element, "name").unwrap_or_default(),
                service_type: attribute(element, "serviceType").unwrap_or_default(),
                base: attribute(element, "base").unwrap_or_default(),
            }),
            "dataset" => {
                let dataset = Open {
                    name: attribute(element, "name").unwrap_or_default(),
                    url_path: attribute(element, "urlPath").filter(|path| !path.is_empty()),
                    size: None,
                    modified: None,
                };
                if empty {
                    finish(&mut catalog, dataset);
                } else {
                    open.push(dataset);
                }
            }
            "catalogRef" => {
                if let Some(href) = attribute(element, "href").filter(|href| !href.is_empty()) {
                    catalog.refs.push(CatalogRef {
                        href,
                        title: attribute(element, "title").or_else(|| attribute(element, "name")),
                    });
                }
            }
            "dataSize" if !empty && !open.is_empty() => {
                field = Some(Field::Size {
                    units: attribute(element, "units").unwrap_or_default(),
                });
                text.clear();
            }
            "date"
                if !empty
                    && !open.is_empty()
                    && attribute(element, "type").as_deref() == Some("modified") =>
            {
                field = Some(Field::Modified);
                text.clear();
            }
            _ => {}
        }
    }
    if root.is_none() {
        return Err(invalid("no root element".into()));
    }
    Ok(catalog)
}

/// List a dataset read whole, if it is a file.
fn finish(catalog: &mut Catalog, dataset: Open) {
    if let Some(url_path) = dataset.url_path {
        catalog.datasets.push(Dataset {
            name: dataset.name,
            url_path,
            size: dataset.size,
            modified: dataset.modified,
        });
    }
}

/// The decoded path of `url`.
fn decoded_path(url: &Url) -> String {
    percent_encoding::percent_decode_str(url.path())
        .decode_utf8_lossy()
        .into_owned()
}

/// Whether directory `dir` is `target` or one of its ancestors; `""` is the
/// root, the ancestor of every directory.
fn within(dir: &str, target: &str) -> bool {
    dir.is_empty()
        || target == dir
        || (target.starts_with(dir) && target.as_bytes().get(dir.len()) == Some(&b'/'))
}

/// What one walk did.
#[derive(Debug, Default)]
struct WalkStats {
    fetched: usize,
    failed: usize,
}

/// The catalogs from the root down to the directories checked.
#[derive(Debug)]
struct Walk {
    label: String,
    root: Url,
    /// The decoded path of the root catalog's directory, ending in `/`.
    root_dir: String,
    /// The template's base, the files' key prefix, without `/` at its ends.
    base: String,
    /// `data_path`'s origin: a catalog's files must be served from it.
    files: url::Origin,
    /// The service whose `base` the files' keys lie under.
    service: FileService,
    /// The catalog URL of each directory under the root a `catalogRef`
    /// led to, by its path relative to the root's directory.
    dirs: BTreeMap<String, Url>,
    max_catalogs: usize,
    /// Whether a catalog listing a planned file's name at another key was
    /// reported (once).
    warned_elsewhere: bool,
}

impl Walk {
    /// The directory of `key` relative to the base, `""` for the base
    /// itself; `None` for a key outside it.
    fn dir_of(&self, key: &str) -> Option<String> {
        let rest = match self.base.is_empty() {
            true => key,
            false => key.strip_prefix(&self.base)?.strip_prefix('/')?,
        };
        Some(rest.rsplit_once('/').map_or("", |(dir, _)| dir).to_string())
    }

    /// The directory a catalog URL stands for, relative to the root's, if
    /// it lies under it on the root's origin.
    fn dir_of_catalog(&self, url: &Url) -> Option<String> {
        if url.origin() != self.root.origin() {
            return None;
        }
        let path = decoded_path(url);
        let rest = path.strip_prefix(&self.root_dir)?;
        Some(
            rest.rsplit_once('/')
                .map_or("", |(dir, _)| dir)
                .trim_matches('/')
                .to_string(),
        )
    }

    /// The deepest directory whose catalog is known on the way to `target`.
    fn deepest_known(&self, target: &str) -> String {
        self.dirs
            .keys()
            .filter(|dir| within(dir, target))
            .max_by_key(|dir| dir.len())
            .cloned()
            .unwrap_or_default()
    }

    fn url_of(&self, dir: &str) -> Url {
        match dir.is_empty() {
            true => self.root.clone(),
            false => self
                .dirs
                .get(dir)
                .cloned()
                .unwrap_or_else(|| self.root.clone()),
        }
    }

    /// Forget `dir` and everything under it: walk to it from its parent
    /// again.
    fn forget(&mut self, dir: &str) {
        self.dirs.retain(|known, _| !within(dir, known));
    }

    /// Keep the directories on the way to `keys` only: the slots in the
    /// window.
    fn retain(&mut self, keys: &[String]) {
        let wanted: Vec<String> = keys.iter().filter_map(|key| self.dir_of(key)).collect();
        self.dirs
            .retain(|dir, _| wanted.iter().any(|target| within(dir, target)));
    }

    /// Remember the refs of `catalog`, read at `url` for `dir`, that lead
    /// toward one of `targets`: below `dir`, under the root.
    fn learn<'a>(
        &mut self,
        dir: &str,
        url: &Url,
        catalog: &Catalog,
        targets: impl Iterator<Item = &'a String> + Clone,
    ) {
        for reference in &catalog.refs {
            let Ok(child) = url.join(&reference.href) else {
                continue;
            };
            let Some(child_dir) = self.dir_of_catalog(&child) else {
                continue;
            };
            let deeper = child_dir != dir && within(dir, &child_dir);
            if deeper && targets.clone().any(|target| within(&child_dir, target)) {
                self.dirs.insert(child_dir, child);
            }
        }
    }

    /// The next directory toward `target` below `dir`, as the refs read so
    /// far name it.
    fn next_toward(&self, dir: &str, target: &str) -> Option<String> {
        self.dirs
            .keys()
            .filter(|known| *known != dir && within(dir, known) && within(known, target))
            .max_by_key(|known| known.len())
            .cloned()
    }

    /// The version `catalog`, read at `url`, states for each of `keys`.
    fn answer(&mut self, catalog: &Catalog, url: &Url, keys: &[&str]) -> Answers {
        let fail = |why: String| -> Answers {
            keys.iter()
                .map(|_| Err(DataServerError::Storage(why.clone())))
                .collect()
        };
        let kind = self.service.service_type();
        let Some(base) = catalog.service_base(self.service) else {
            return fail(format!(
                "catalog {url} names no {kind} service: its files cannot be read through one"
            ));
        };
        let base = match url.join(base) {
            Ok(base) => base,
            Err(e) => return fail(format!("catalog {url}: {kind} base `{base}`: {e}")),
        };
        if base.origin() != self.files {
            return fail(format!(
                "catalog {url} serves its files from {}, not from data_path's {}",
                base.origin().ascii_serialization(),
                self.files.ascii_serialization()
            ));
        }
        let mut prefix = decoded_path(&base).trim_start_matches('/').to_string();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        let listed: HashMap<String, ObjectVersion> = catalog
            .datasets
            .iter()
            .map(|dataset| {
                let key = format!("{prefix}{}", dataset.url_path.trim_start_matches('/'));
                let version = ObjectVersion {
                    size: dataset.size.unwrap_or(0),
                    last_modified: dataset.modified,
                    e_tag: None,
                };
                (ObjectPath::from(key.as_str()).to_string(), version)
            })
            .collect();
        let answers = keys
            .iter()
            .map(|key| Ok(listed.get(*key).cloned()))
            .collect();
        if !self.warned_elsewhere {
            // A file the template names, listed at another key: data_path
            // and catalog_url name different directories.
            let elsewhere = keys.iter().find_map(|key| {
                let name = key.rsplit('/').next()?;
                let found = listed.keys().find(|listed| {
                    listed.rsplit('/').next() == Some(name) && listed.as_str() != *key
                })?;
                Some((key.to_string(), found.clone()))
            });
            if let Some((key, found)) = elsewhere {
                self.warned_elsewhere = true;
                tracing::warn!(
                    "[{}] catalog {url} lists `{found}`, not `{key}`: does data_path name \
                     the {} directory of catalog_url?",
                    self.label,
                    self.service.service_type()
                );
            }
        }
        answers
    }

    /// Answer `keys` from the catalogs of their directories, walking to
    /// each from the deepest directory already known. A directory the walk
    /// cannot reach (no ref leads there yet) holds nothing: `Ok(None)`. A
    /// catalog that cannot be read fails its keys, and a catalog URL
    /// remembered for it is forgotten, so the next poll walks to it again.
    fn lookup(
        &mut self,
        store: &DataStore,
        keys: &[ObjectPath],
        stats: &mut WalkStats,
    ) -> Result<Answers, DataServerError> {
        let mut answers: Vec<Option<Result<Option<ObjectVersion>, DataServerError>>> =
            keys.iter().map(|_| None).collect();
        let mut targets: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, key) in keys.iter().enumerate() {
            match self.dir_of(key.as_ref()) {
                Some(dir) => targets.entry(dir).or_default().push(i),
                None => {
                    answers[i] = Some(Err(DataServerError::Storage(format!(
                        "`{key}` is not under the template's base `{}`",
                        self.base
                    ))))
                }
            }
        }
        // Each target directory and the directory whose catalog to read
        // next on the way to it.
        let mut cursor: BTreeMap<String, String> = targets
            .keys()
            .map(|target| (target.clone(), self.deepest_known(target)))
            .collect();
        while !cursor.is_empty() {
            let room = self.max_catalogs.saturating_sub(stats.fetched);
            if room == 0 || ds_core::deadline::check().is_err() {
                let why = match room {
                    0 => format!(
                        "read {} catalogs this poll, the most one poll reads; the rest wait \
                         for the next",
                        stats.fetched
                    ),
                    _ => "the scan's time ran out".to_string(),
                };
                for target in std::mem::take(&mut cursor).into_keys() {
                    fail(&mut answers, &targets[&target], &why);
                }
                break;
            }
            let dirs: Vec<String> = cursor
                .values()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .take(room)
                .collect();
            let urls: Vec<Url> = dirs.iter().map(|dir| self.url_of(dir)).collect();
            let paths: Vec<ObjectPath> = urls
                .iter()
                .map(|url| ObjectPath::from(decoded_path(url).trim_start_matches('/')))
                .collect();
            let bodies = store.get_documents(
                &paths,
                CATALOG_CONCURRENCY,
                MAX_CATALOG_BYTES as u64,
                FetchBudget::Request,
            )?;
            stats.fetched += dirs.len();
            for ((dir, url), body) in dirs.iter().zip(&urls).zip(bodies) {
                tracing::debug!("[{}] read catalog {url}", self.label);
                let waiting: Vec<String> = cursor
                    .iter()
                    .filter(|(_, at)| *at == dir)
                    .map(|(target, _)| target.clone())
                    .collect();
                let catalog = match body.and_then(|body| parse(&body)) {
                    Ok(catalog) => catalog,
                    Err(e) => {
                        stats.failed += 1;
                        if !dir.is_empty() {
                            self.forget(dir);
                        }
                        let why = format!("catalog {url}: {e}");
                        for target in waiting {
                            cursor.remove(&target);
                            fail(&mut answers, &targets[&target], &why);
                        }
                        continue;
                    }
                };
                self.learn(dir, url, &catalog, waiting.iter());
                for target in waiting {
                    if target == *dir {
                        cursor.remove(&target);
                        let indices = &targets[&target];
                        let asked: Vec<&str> = indices.iter().map(|&i| keys[i].as_ref()).collect();
                        let found = self.answer(&catalog, url, &asked);
                        for (&i, found) in indices.iter().zip(found) {
                            answers[i] = Some(found);
                        }
                    } else if let Some(next) = self.next_toward(dir, &target) {
                        cursor.insert(target, next);
                    } else {
                        // No ref leads there: the run is not out yet.
                        cursor.remove(&target);
                        for &i in &targets[&target] {
                            answers[i] = Some(Ok(None));
                        }
                    }
                }
            }
        }
        Ok(answers
            .into_iter()
            .map(|answer| {
                answer.unwrap_or_else(|| Err(DataServerError::Storage("not looked up".into())))
            })
            .collect())
    }
}

/// Fail the keys at `indices` with `why`.
fn fail(
    answers: &mut [Option<Result<Option<ObjectVersion>, DataServerError>>],
    indices: &[usize],
    why: &str,
) {
    for &i in indices {
        answers[i] = Some(Err(DataServerError::Storage(why.to_string())));
    }
}

/// The version a HEAD found for a file a catalog lists.
#[derive(Debug, Clone)]
struct Exact {
    /// The catalog's version of the file when it was HEADed.
    listed: ObjectVersion,
    /// `None`: the file server answered 404 for a file the catalog lists.
    version: Option<ObjectVersion>,
    /// A reader found the file replaced: HEAD it again.
    stale: bool,
}

/// A [`TemplateProber`] whose checks THREDDS catalogs answer. See the
/// [module docs](self).
#[derive(Debug)]
pub struct CatalogProber {
    prober: TemplateProber,
    walk: Walk,
    /// Each listed file's version, by key.
    exact: HashMap<String, Exact>,
}

impl CatalogProber {
    /// A prober of the files `spec` names, found from the THREDDS catalog
    /// at `catalog_url` (`…/catalog.xml`) and served under `files_url`, the
    /// HTTP source the spec's `base` is the path of. A config error names
    /// what cannot be used.
    pub fn new(
        spec: TemplateSpec,
        catalog_url: &str,
        files_url: &str,
    ) -> Result<Self, DataServerError> {
        let invalid =
            |why: String| DataServerError::Config(format!("[{}] catalog_url: {why}", spec.label));
        let root = Url::parse(catalog_url).map_err(|e| invalid(format!("`{catalog_url}`: {e}")))?;
        if !matches!(root.scheme(), "http" | "https") || root.host_str().is_none() {
            return Err(invalid(format!("`{catalog_url}` is not an http(s) URL")));
        }
        if root.query().is_some() || root.fragment().is_some() {
            return Err(invalid(format!(
                "`{catalog_url}` has a query or fragment; name the catalog.xml itself"
            )));
        }
        let path = decoded_path(&root);
        if !path.ends_with(".xml") {
            return Err(invalid(format!(
                "`{catalog_url}` is not a catalog's .xml (the .html page is for browsers)"
            )));
        }
        let files = Url::parse(files_url)
            .map_err(|e| invalid(format!("data_path `{files_url}`: {e}")))?
            .origin();
        let root_dir = match path.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/"),
            None => "/".to_string(),
        };
        let walk = Walk {
            label: spec.label.clone(),
            base: spec.base.trim_matches('/').to_string(),
            root,
            root_dir,
            files,
            service: FileService::HttpServer,
            dirs: BTreeMap::new(),
            max_catalogs: MAX_CATALOGS_PER_POLL,
            warned_elsewhere: false,
        };
        Ok(Self {
            prober: TemplateProber::new(spec)?,
            walk,
            exact: HashMap::new(),
        })
    }

    /// This prober, its files read through `service` ([`FileService`]):
    /// their keys lie under that service's `base`.
    #[must_use]
    pub fn with_service(mut self, service: FileService) -> Self {
        self.walk.service = service;
        self
    }

    pub fn spec(&self) -> &TemplateSpec {
        self.prober.spec()
    }

    /// The key of `lead`'s file in the slot at `time`.
    pub fn key(&self, time: DateTime<Utc>, lead: Option<u32>) -> String {
        self.prober.key(time, lead)
    }

    /// The root catalog's URL.
    pub fn catalog_url(&self) -> &Url {
        &self.walk.root
    }

    /// The settled files of every published slot whose version a HEAD has
    /// found, oldest slot first, each slot's in lead order: with that
    /// version, the identity a conditional range read checks. Through
    /// OPeNDAP, every settled file, with the catalog's version.
    pub fn files(&self) -> Vec<ProbedFile> {
        if self.walk.service == FileService::OpenDap {
            return self.prober.files();
        }
        self.prober
            .files()
            .into_iter()
            .filter_map(|file| {
                let exact = self.exact.get(&file.key)?;
                let version = exact
                    .version
                    .clone()
                    .filter(|_| exact.listed == file.version)?;
                Some(ProbedFile { version, ..file })
            })
            .collect()
    }

    /// Check the settled file `key` again on the next poll: its catalog
    /// entry, as [`TemplateProber::recheck`] does, and its version, with a
    /// HEAD. A reader found that the key no longer names the version it was
    /// listed with. Returns whether `key` is a settled file in the window.
    pub fn recheck(&mut self, key: &str) -> bool {
        if let Some(exact) = self.exact.get_mut(key) {
            exact.stale = true;
        }
        self.prober.recheck(key)
    }

    /// Poll once at `now`: the template prober's poll, its checks answered
    /// by the catalogs `catalogs` reads (a store on `catalog_url`'s
    /// origin), then a HEAD through `files` of each listed file whose
    /// version is not known yet (none through OPeNDAP). Call it where
    /// [`DataStore::head`] may be
    /// called: from the background poll runtime. The report's
    /// [`ProbeReport::checked`] counts the keys looked up,
    /// [`ProbeReport::catalogs`] the catalogs read and
    /// [`ProbeReport::heads`] the HEADs made; `added` and `withdrawn` are
    /// what joined and left [`Self::files`], with the HEADs' versions.
    pub fn poll(
        &mut self,
        catalogs: &DataStore,
        files: &DataStore,
        now: DateTime<Utc>,
    ) -> Result<ProbeReport, DataServerError> {
        let before = self.files();
        let mut stats = WalkStats::default();
        let walk = &mut self.walk;
        let mut report = self
            .prober
            .poll_with(now, |keys| walk.lookup(catalogs, keys, &mut stats))?;
        self.walk.retain(&self.prober.slot_sentinels());
        report.catalogs = stats.fetched;
        report.catalogs_failed = stats.failed;
        // `failed` counted lookups; from here it counts HEADs.
        report.failed = 0;

        let listed = self.prober.files();
        let unknown: Vec<&ProbedFile> = listed
            .iter()
            .filter(|_| self.walk.service == FileService::HttpServer)
            .filter(|file| {
                self.exact
                    .get(&file.key)
                    .is_none_or(|exact| exact.stale || exact.listed != file.version)
            })
            .collect();
        if !unknown.is_empty() {
            let paths: Vec<ObjectPath> = unknown
                .iter()
                .map(|file| ObjectPath::from(file.key.as_str()))
                .collect();
            let answers = files.head_many(&paths, PROBE_CONCURRENCY)?;
            report.heads = paths.len();
            let label = &self.walk.label;
            for (file, answer) in unknown.into_iter().zip(answers) {
                match answer {
                    Ok(found) => {
                        if found.is_none() {
                            tracing::warn!(
                                "[{label}] the catalog lists `{}`, but the file server has no \
                                 such file",
                                file.key
                            );
                        }
                        self.exact.insert(
                            file.key.clone(),
                            Exact {
                                listed: file.version.clone(),
                                version: found.as_ref().map(ObjectVersion::from),
                                stale: false,
                            },
                        );
                    }
                    Err(e) => {
                        // Tried again next poll; a stale file keeps its
                        // version meanwhile, as a failed recheck HEAD does.
                        tracing::warn!("[{label}] HEAD `{}` failed: {e}", file.key);
                        report.failed += 1;
                    }
                }
            }
        }
        let listed: std::collections::HashSet<&str> =
            listed.iter().map(|file| file.key.as_str()).collect();
        self.exact.retain(|key, _| listed.contains(key.as_str()));

        let after = self.files();
        let missing = |from: &[ProbedFile], file: &ProbedFile| !from.contains(file);
        report.added = after
            .iter()
            .filter(|file| missing(&before, file))
            .cloned()
            .collect();
        report.withdrawn = before
            .iter()
            .filter(|file| missing(&after, file))
            .cloned()
            .collect();
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
