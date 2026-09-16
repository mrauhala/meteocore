//! CAP data sources: a local directory of `.xml` files, or an Atom/RSS feed
//! whose entries link to individual CAP documents.
//!
//! Both go through `ds-storage` (the sync object-store bridge), so this module
//! must only ever be driven from the background poll runtime — never a request
//! handler, never a rayon pool (`feedback_storage_sync_bridge_misuse`). The feed
//! path fetches the linked CAP documents with `DataStore::get_many` (bounded
//! concurrency, per-object timeout) rather than a sequential blocking loop.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use url::Url;

use ds_core::error::DataServerError;
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::{build_store, DataStore};

use crate::parser::{parse_document, CapAlert};
use crate::wis2::Wis2CapSource;

/// Per-object byte cap for a CAP document / feed index (geometry-bomb guard).
const MAX_DOC_BYTES: u64 = 8 * 1024 * 1024;
/// Concurrency for the bounded `get_many` fetches.
const FETCH_CONCURRENCY: usize = 8;
/// Cap on `.xml` files pulled from a local directory per scan.
const MAX_LOCAL_FILES: usize = 50_000;
/// Cap on entry links followed from a feed index per fetch.
const MAX_FEED_ENTRIES: usize = 2_000;
/// Cap on query-bearing entry links (each needs its own HTTP client — object
/// store paths can't carry a query string, so these can't share a store).
const MAX_QUERY_ENTRIES: usize = 64;

/// A resolved CAP data source. Constructed without any network I/O; the actual
/// scan/fetch happens in [`Source::load_at`].
pub enum Source {
    /// Local directory of CAP `.xml` files.
    Local {
        store: DataStore,
        base: ObjectPath,
        cache: Mutex<DocumentCache>,
    },
    /// Atom/RSS feed index → linked CAP documents.
    Feed {
        cache: Mutex<DocumentCache>,
        index_store: DataStore,
        index_path: ObjectPath,
        feed_url: String,
        /// SSRF allowlist of extra URL prefixes (the feed's own origin is always
        /// allowed); see [`CapConfig::feed_allowlist`](ds_core::config::CapConfig).
        allowlist: Vec<String>,
    },
    /// WIS2 Global Broker subscription: alerts pushed as notifications and
    /// accumulated in memory (see [`crate::wis2`]). `load()` is a snapshot of
    /// the accumulator — no I/O; the broker task feeds it from `poll_loop`.
    Wis2 {
        source: Arc<Wis2CapSource>,
        topics: Vec<String>,
    },
}

impl Source {
    /// Build the source's object store(s). No network calls — only client setup.
    pub fn build(
        data_path: Option<&str>,
        feed_url: Option<&str>,
        feed_allowlist: &[String],
    ) -> Result<Self, DataServerError> {
        match (data_path, feed_url) {
            (Some(path), None) => {
                let (store, base) = build_store(path)?;
                Ok(Source::Local {
                    store,
                    base,
                    cache: Mutex::default(),
                })
            }
            (None, Some(url)) => {
                let (index_store, index_path) = build_store(url)?;
                Ok(Source::Feed {
                    cache: Mutex::default(),
                    index_store,
                    index_path,
                    feed_url: url.to_string(),
                    allowlist: feed_allowlist.to_vec(),
                })
            }
            _ => Err(DataServerError::Config(
                "cap source requires exactly one of data_path or feed_url".into(),
            )),
        }
    }

    /// A short human label for logging.
    pub fn label(&self) -> String {
        match self {
            Source::Local { base, .. } => format!("local dir '{base}'"),
            Source::Feed { feed_url, .. } => format!("feed '{feed_url}'"),
            Source::Wis2 { topics, .. } => format!("wis2 {} topic filter(s)", topics.len()),
        }
    }

    /// Fetch and parse every CAP document this source exposes (WIS2: the
    /// accumulated set after eviction as of `now`).
    pub fn load_at(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<SourceLoad, DataServerError> {
        match self {
            Source::Local { store, base, cache } => load_local(store, base, cache),
            Source::Feed {
                cache,
                index_store,
                index_path,
                feed_url,
                allowlist,
            } => load_feed(index_store, index_path, feed_url, allowlist, cache),
            Source::Wis2 { source, .. } => Ok(SourceLoad {
                alerts: source.snapshot(now),
                failed_documents: 0,
            }),
        }
    }

    /// The WIS2 accumulator, when this is a WIS2 source.
    pub fn wis2(&self) -> Option<&Arc<Wis2CapSource>> {
        match self {
            Source::Wis2 { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// List `.xml` files under `base`, fetch them (bounded), and parse each.
fn load_local(
    store: &DataStore,
    base: &ObjectPath,
    cache: &Mutex<DocumentCache>,
) -> Result<SourceLoad, DataServerError> {
    let mut paths: Vec<ObjectPath> = store
        .list(base)?
        .into_iter()
        .map(|m| m.location)
        .filter(|p| p.as_ref().to_ascii_lowercase().ends_with(".xml"))
        .collect();
    paths.sort_unstable();
    if paths.len() > MAX_LOCAL_FILES {
        tracing::warn!(
            "cap local scan capped at {MAX_LOCAL_FILES} of {} .xml files",
            paths.len()
        );
        paths.truncate(MAX_LOCAL_FILES);
    }
    let documents = fetch_and_parse(store, &paths, "");
    Ok(cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .update(documents))
}

/// Fetch the feed index, extract CAP document links, and fetch + parse them.
fn load_feed(
    index_store: &DataStore,
    index_path: &ObjectPath,
    feed_url: &str,
    allowlist: &[String],
    cache: &Mutex<DocumentCache>,
) -> Result<SourceLoad, DataServerError> {
    // Both index and document fetches use ds-storage's no-redirect HTTP
    // connector. The entry allowlist below validates linked request URLs;
    // redirects cannot bypass that boundary (#431).
    // Fetch the index through the size-guarded path (HEAD-checks the body
    // against MAX_DOC_BYTES before pulling it into memory), so an oversized or
    // malicious feed can't exhaust the heap before a post-hoc length check —
    // the same cap the entry fetches use.
    let index_bytes = match index_store
        .get_many(std::slice::from_ref(index_path), 1, Some(MAX_DOC_BYTES))?
        .into_iter()
        .next()
    {
        Some(Ok(bytes)) => bytes,
        Some(Err(e)) => return Err(e),
        None => {
            return Err(DataServerError::Engine(
                "empty CAP feed fetch result".into(),
            ))
        }
    };
    let index_xml = String::from_utf8_lossy(&index_bytes);
    validate_feed(&index_xml)?;

    let base = Url::parse(feed_url)
        .map_err(|e| DataServerError::Engine(format!("invalid cap feed_url '{feed_url}': {e}")))?;
    let feed_origin = origin_of(&base);
    let links = extract_feed_links(&index_xml);

    // Resolve to absolute URLs, SSRF-filter, dedup, and cap. An entry is fetched
    // only if it shares the feed's exact origin (scheme+host+port) or matches a
    // configured allowlist prefix — so a compromised feed can't redirect the
    // server to cloud-metadata / internal hosts (mirrors STAC's allowlist).
    let mut resolved: Vec<Url> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut blocked = 0usize;
    for link in links {
        if let Ok(abs) = base.join(&link) {
            if !matches!(abs.scheme(), "http" | "https") {
                continue;
            }
            if !is_allowed_entry(&abs, &feed_origin, allowlist) {
                blocked += 1;
                continue;
            }
            if seen.insert(abs.as_str().to_string()) {
                resolved.push(abs);
                if resolved.len() >= MAX_FEED_ENTRIES {
                    tracing::warn!("cap feed entry links capped at {MAX_FEED_ENTRIES}");
                    break;
                }
            }
        }
    }
    if blocked > 0 {
        tracing::warn!(
            "cap feed '{feed_url}': dropped {blocked} cross-origin entry link(s) not on the \
             feed origin or allowlist (SSRF guard)"
        );
    }

    if resolved.is_empty() {
        tracing::warn!("cap feed '{feed_url}' yielded no CAP document links");
        return Ok(cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .update(Vec::new()));
    }

    // Group query-less URLs by origin so each origin's docs fetch via one
    // bounded `get_many`. URLs carrying a query string can't be expressed as an
    // object-store path, so they fall back to individual fetches (one HTTP client
    // each) — capped tighter than the overall entry cap so a feed of thousands of
    // query-bearing links can't open thousands of connections per poll.
    let mut by_origin: BTreeMap<String, Vec<ObjectPath>> = BTreeMap::new();
    let mut with_query: Vec<Url> = Vec::new();
    for u in &resolved {
        if u.query().is_some() {
            if with_query.len() < MAX_QUERY_ENTRIES {
                with_query.push(u.clone());
            }
        } else {
            let origin = origin_of(u);
            by_origin
                .entry(origin)
                .or_default()
                .push(ObjectPath::from(u.path().trim_start_matches('/')));
        }
    }
    let query_total = resolved.iter().filter(|u| u.query().is_some()).count();
    if query_total > MAX_QUERY_ENTRIES {
        tracing::warn!(
            "cap feed '{feed_url}': {query_total} query-bearing entry links — only the first \
             {MAX_QUERY_ENTRIES} are fetched (each needs its own connection)"
        );
    }

    // One HTTP client per origin per poll. After the SSRF filter the common case
    // is a single origin (the feed's own), so this is one client per poll cycle
    // (every `poll_interval_secs`, default 300s) — negligible. `DataStore` has no
    // cross-call client reuse; pooling across polls is a future optimisation.
    let mut documents = Vec::new();
    for (origin, paths) in by_origin {
        match build_store(&origin) {
            Ok((store, _)) => documents.extend(fetch_and_parse(&store, &paths, &origin)),
            Err(e) => {
                tracing::warn!("cap feed: cannot build store for origin '{origin}': {e}");
                documents.extend(paths.iter().map(|p| (format!("{origin}/{p}"), None)));
            }
        }
    }
    // Rare query-bearing links: fetch individually through the size-guarded
    // `get_many` (HEAD-checks the body against MAX_DOC_BYTES before pulling it),
    // still bounded by the entry count cap.
    for u in with_query {
        let fetched = build_store(u.as_str()).and_then(|(store, path)| {
            store
                .get_many(std::slice::from_ref(&path), 1, Some(MAX_DOC_BYTES))?
                .into_iter()
                .next()
                .unwrap_or_else(|| Err(DataServerError::Engine("empty fetch result".into())))
        });
        let parsed = match fetched {
            Ok(bytes) => parse_bytes(&bytes, u.as_str()),
            Err(e) => {
                tracing::warn!("cap feed doc '{u}' fetch failed: {e}");
                None
            }
        };
        documents.push((u.to_string(), parsed));
    }

    Ok(cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .update(documents))
}

/// Last good document copies. A failed GET keeps that document's previous
/// alerts; an object absent from a successfully read index is actually removed.
#[derive(Default)]
pub struct DocumentCache {
    documents: BTreeMap<String, Vec<CapAlert>>,
}

pub struct SourceLoad {
    pub alerts: Vec<CapAlert>,
    pub failed_documents: usize,
}

type DocumentResult = (String, Option<Vec<CapAlert>>);

impl DocumentCache {
    fn update(&mut self, results: Vec<DocumentResult>) -> SourceLoad {
        let listed: std::collections::HashSet<_> =
            results.iter().map(|(key, _)| key.as_str()).collect();
        self.documents
            .retain(|key, _| listed.contains(key.as_str()));
        let mut failed_documents = 0;
        for (key, result) in results {
            if let Some(alerts) = result {
                self.documents.insert(key, alerts);
            } else {
                failed_documents += 1;
            }
        }
        SourceLoad {
            alerts: self.documents.values().flatten().cloned().collect(),
            failed_documents,
        }
    }
}

/// Fetch documents concurrently, retaining an explicit failure for each URL.
fn fetch_and_parse(store: &DataStore, paths: &[ObjectPath], origin: &str) -> Vec<DocumentResult> {
    if paths.is_empty() {
        return Vec::new();
    }
    match store.get_many(paths, FETCH_CONCURRENCY, Some(MAX_DOC_BYTES)) {
        Ok(results) => paths
            .iter()
            .zip(results)
            .map(|(path, result)| {
                let parsed = match result {
                    Ok(bytes) => parse_bytes(&bytes, path.as_ref()),
                    Err(e) => {
                        tracing::warn!("cap: fetch of '{path}' failed: {e}");
                        None
                    }
                };
                (format!("{origin}/{path}"), parsed)
            })
            .collect(),
        Err(e) => {
            tracing::warn!("cap: batch fetch failed: {e}");
            paths
                .iter()
                .map(|p| (format!("{origin}/{p}"), None))
                .collect()
        }
    }
}

fn parse_bytes(bytes: &[u8], label: &str) -> Option<Vec<CapAlert>> {
    if let Err(e) = std::str::from_utf8(bytes) {
        tracing::warn!("cap: '{label}' is not valid UTF-8 ({e}) — parsing lossily; free-text fields may be garbled");
    }
    let xml = String::from_utf8_lossy(bytes);
    match parse_document(&xml) {
        Ok(parsed) if !parsed.is_empty() => Some(parsed),
        Ok(_) => {
            tracing::warn!("cap: '{label}' contained no CAP alerts");
            None
        }
        Err(e) => {
            tracing::warn!("cap: parse of '{label}' failed: {e}");
            None
        }
    }
}

/// Never interpret a truncated index or an HTML error response as an empty
/// alert feed. Link extraction remains tolerant of individual malformed links.
fn validate_feed(xml: &str) -> Result<(), DataServerError> {
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut root = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if root.is_none() {
                    root = Some(local(e.local_name().as_ref()));
                }
                depth += 1;
            }
            Ok(Event::Empty(e)) if root.is_none() => {
                root = Some(local(e.local_name().as_ref()));
            }
            Ok(Event::End(_)) => {
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(DataServerError::Engine(format!(
                    "invalid CAP feed XML: {e}"
                )))
            }
            _ => {}
        }
    }
    if depth != 0 || !matches!(root.as_deref(), Some("feed" | "rss" | "RDF")) {
        return Err(DataServerError::Engine(
            "invalid or incomplete CAP feed index".into(),
        ));
    }
    Ok(())
}

fn origin_of(u: &Url) -> String {
    match u.port() {
        Some(p) => format!("{}://{}:{}", u.scheme(), u.host_str().unwrap_or(""), p),
        None => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")),
    }
}

/// SSRF gate: an entry link may be fetched only if it shares the feed's **exact**
/// origin (compared as origin strings, never a prefix — `https://feed` must not
/// admit `https://feed.evil.com`) or starts with a configured allowlist prefix
/// (the operator's explicit opt-in, mirroring STAC's allowlist semantics).
fn is_allowed_entry(u: &Url, feed_origin: &str, allowlist: &[String]) -> bool {
    origin_of(u) == feed_origin || allowlist.iter().any(|p| u.as_str().starts_with(p))
}

/// Extract candidate CAP-document links from an Atom/RSS feed index.
///
/// One link per `<entry>` (Atom) / `<item>` (RSS): a `<link>` whose `type`
/// names `cap+xml` wins; otherwise the first `<link href>` / `<link>` text /
/// `<id>` / `<guid>` that looks like a URL. Links are returned raw (possibly
/// relative) — the caller resolves them against the feed base.
pub fn extract_feed_links(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    // No `trim_text(true)` — see `parser::parse_document` for why per-event
    // trimming is wrong once entity references split a text node. Each leaf is
    // trimmed whole below.
    let mut links: Vec<String> = Vec::new();
    let mut in_entry = false;
    // Per-entry candidate state.
    let mut cap_typed: Option<String> = None;
    let mut fallback: Option<String> = None;
    let mut text = String::new();
    let mut cur_elem = String::new();

    let pick_link = |e: &quick_xml::events::BytesStart,
                     cap_typed: &mut Option<String>,
                     fallback: &mut Option<String>| {
        let mut href: Option<String> = None;
        let mut typ: Option<String> = None;
        let mut rel: Option<String> = None;
        for attr in e.attributes().flatten() {
            let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
            // Replaces the deprecated `unescape_value`; also applies XML
            // attribute-value normalization. `Implicit1_0` — the declaration is
            // not inspected and 1.0 is the assumed default (see `parser`).
            let val = attr
                .normalized_value(XmlVersion::Implicit1_0)
                .map(|v| v.into_owned())
                .unwrap_or_default();
            match key.as_str() {
                "href" => href = Some(val),
                "type" => typ = Some(val.to_ascii_lowercase()),
                "rel" => rel = Some(val.to_ascii_lowercase()),
                _ => {}
            }
        }
        if let Some(h) = href {
            if typ.as_deref().is_some_and(|t| t.contains("cap")) {
                cap_typed.get_or_insert(h);
            } else if rel.as_deref().map(|r| r == "alternate").unwrap_or(true) {
                // alternate or unspecified rel — a usable fallback.
                fallback.get_or_insert(h);
            }
        }
    };

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local(e.local_name().as_ref());
                if name == "entry" || name == "item" {
                    in_entry = true;
                    cap_typed = None;
                    fallback = None;
                } else if in_entry && name == "link" {
                    pick_link(&e, &mut cap_typed, &mut fallback);
                }
                cur_elem = name;
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                // Atom `<link .../>` is an empty element.
                let name = local(e.local_name().as_ref());
                if in_entry && name == "link" {
                    pick_link(&e, &mut cap_typed, &mut fallback);
                }
            }
            Ok(Event::Text(e)) => {
                if let Ok(t) = e.xml10_content() {
                    text.push_str(&t);
                }
            }
            // A `<link>https://host/a&amp;b</link>` arrives as three events
            // since quick-xml 0.38; without this the URL loses its `&`.
            Ok(Event::GeneralRef(r)) => crate::parser::push_general_ref(&mut text, &r),
            Ok(Event::End(e)) => {
                let name = local(e.local_name().as_ref());
                if in_entry {
                    // RSS <link>text</link>, or <id>/<guid> URLs as fallbacks.
                    let leaf = text.trim();
                    if (cur_elem == "link" || cur_elem == "id" || cur_elem == "guid")
                        && looks_like_url(leaf)
                    {
                        fallback.get_or_insert_with(|| leaf.to_string());
                    }
                    if name == "entry" || name == "item" {
                        if let Some(l) = cap_typed.take().or_else(|| fallback.take()) {
                            links.push(l);
                        }
                        in_entry = false;
                    }
                }
                text.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // tolerate malformed feeds: return what we have
            _ => {}
        }
    }
    links
}

fn local(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_index_is_not_a_successful_empty_feed() {
        for xml in [
            "<feed><entry>",
            "<html>Unavailable</html>",
            "",
            "<feed><entry></feed>",
        ] {
            assert!(validate_feed(xml).is_err(), "{xml}");
        }
        for xml in ["<feed/>", "<feed></feed>", "<rss><channel/></rss>"] {
            assert!(validate_feed(xml).is_ok(), "{xml}");
        }
    }

    #[test]
    fn atom_prefers_cap_typed_link() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <entry>
            <id>https://example.org/alerts/1</id>
            <link rel="alternate" type="text/html" href="https://example.org/alerts/1.html"/>
            <link type="application/cap+xml" href="https://example.org/alerts/1.cap.xml"/>
          </entry>
        </feed>"#;
        let links = extract_feed_links(atom);
        assert_eq!(
            links,
            vec!["https://example.org/alerts/1.cap.xml".to_string()]
        );
    }

    #[test]
    fn rss_link_text_keeps_escaped_ampersands() {
        // Same quick-xml 0.38+ split as in `parser` (#584). A feed whose item
        // link carries a query string is the common case, and losing the `&`
        // turns a working CAP URL into a 404 — the failure would surface far
        // downstream as "feed yields no alerts".
        let rss = r#"<rss version="2.0"><channel>
          <item><link>https://example.org/cap?id=9&amp;fmt=xml</link></item>
        </channel></rss>"#;
        assert_eq!(
            extract_feed_links(rss),
            vec!["https://example.org/cap?id=9&fmt=xml".to_string()]
        );
    }

    #[test]
    fn atom_falls_back_to_alternate_href() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <entry>
            <id>tag:example.org,2026:1</id>
            <link href="cap/1.xml"/>
          </entry>
        </feed>"#;
        // Relative href kept raw for caller resolution.
        assert_eq!(extract_feed_links(atom), vec!["cap/1.xml".to_string()]);
    }

    #[test]
    fn rss_uses_item_link_text() {
        let rss = r#"<rss version="2.0"><channel>
          <title>feed</title>
          <link>https://example.org</link>
          <item><title>x</title><link>https://example.org/cap/9.xml</link></item>
        </channel></rss>"#;
        // The channel-level <link> is outside any <item>, so it is not collected.
        assert_eq!(
            extract_feed_links(rss),
            vec!["https://example.org/cap/9.xml".to_string()]
        );
    }

    #[test]
    fn ssrf_filter_allows_feed_origin_and_allowlist_only() {
        let feed_origin = "https://feeds.example.org";
        let allow = vec!["https://cdn.example.net/cap/".to_string()];
        let chk = |u: &str| is_allowed_entry(&Url::parse(u).unwrap(), feed_origin, &allow);
        // Same origin as the feed → allowed.
        assert!(chk("https://feeds.example.org/alerts/1.xml"));
        // Allowlisted prefix → allowed.
        assert!(chk("https://cdn.example.net/cap/1.xml"));
        // Cloud metadata / internal hosts → blocked.
        assert!(!chk("http://169.254.169.254/latest/meta-data/"));
        assert!(!chk("http://localhost:8080/admin"));
        // Look-alike host must NOT pass the exact-origin check (prefix-safety).
        assert!(!chk("https://feeds.example.org.evil.com/x.xml"));
        // Different scheme / non-default port is a different origin → blocked.
        assert!(!chk("http://feeds.example.org/alerts/1.xml"));
        assert!(!chk("https://feeds.example.org:8443/alerts/1.xml"));
        // An explicit *default* port is the same origin (the `url` crate
        // normalises `:443`/`:80` away), so it must still be allowed.
        assert!(chk("https://feeds.example.org:443/alerts/1.xml"));
    }

    #[test]
    fn multiple_entries_yield_multiple_links() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <entry><link type="application/cap+xml" href="a.xml"/></entry>
          <entry><link type="application/cap+xml" href="b.xml"/></entry>
        </feed>"#;
        assert_eq!(extract_feed_links(atom), vec!["a.xml", "b.xml"]);
    }
}
