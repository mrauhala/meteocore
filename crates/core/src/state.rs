//! Engine state snapshots (#1000): a backend-neutral store and the write
//! policy every engine shares.
//!
//! An engine that accumulates state in memory from a push feed (CAP over
//! WIS2 today, the BUFR WIS2 store next, #1002) cannot re-read that state
//! from its source after a restart: the broker does not replay what it
//! already delivered. Such an engine snapshots the state through a
//! [`StateStore`] under a key of its own ([`collection_key`]:
//! `<collection id>.<kind>`, `kind` = `cap`, `bufr`, …) and restores it when
//! it is built.
//!
//! - [`StateStore`] is the backend: whole blobs per key, an atomic replace,
//!   no partial updates. Engines hold an `Arc<dyn StateStore>` and a key,
//!   never a path, so another backend (Redis, …) slots in without touching
//!   them. [`FileStateStore`] is the one backend today (`[server]
//!   state_dir`).
//! - [`StateWriter`] is the write policy, independent of the backend: write
//!   only when the engine's state revision moved, at most once per
//!   interval, rewrite an unchanged state now and then so the snapshot's own
//!   timestamp says when the server was last alive, and surface failures at
//!   a bounded rate.
//!
//! The snapshot format, its timestamp and the logging belong to the engine.
//! Persistence is best effort: a failed write keeps the previous snapshot
//! and the engine serves on; an unreadable or corrupt snapshot is a cold
//! start, never a load failure.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Why a [`StateStore`] call or a snapshot write failed.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The file backend's I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Another backend's failure, already formatted for a log line.
    #[error("{0}")]
    Backend(String),
    /// The engine could not encode its snapshot (nothing reached the store).
    #[error("snapshot encoding failed: {0}")]
    Encode(String),
}

/// Where engine snapshots live. Object safe: engines hold an
/// `Arc<dyn StateStore>`, and the server picks the backend at boot.
///
/// A snapshot is one whole blob per key: [`Self::save`] replaces it
/// atomically (a later [`Self::load`] sees the old blob or the new one,
/// never a mix, and a failed save leaves the old one in place). Two writers
/// of one key — an engine being replaced by a reload and its successor —
/// may race; the last complete save wins. Calls block: engines make them
/// from the background poll runtime, at build, or at shutdown.
pub trait StateStore: Send + Sync + fmt::Debug {
    /// The blob stored under `key`, `Ok(None)` when there is none yet.
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, StateError>;
    /// Replace the blob under `key` with `bytes`, atomically.
    fn save(&self, key: &str, bytes: &[u8]) -> Result<(), StateError>;
    /// Where `key` lives, for log lines (the file backend: its path).
    fn describe(&self, key: &str) -> String;
}

/// The store key of one collection's snapshot: `<collection id>.<kind>`.
/// `kind` names the engine's format (`cap`, `bufr`) and contains no `.`, so
/// two collections or two engine kinds never share a key.
pub fn collection_key(collection_id: &str, kind: &str) -> String {
    debug_assert!(!kind.is_empty() && !kind.contains('.'), "kind {kind:?}");
    format!("{collection_id}.{kind}")
}

/// The file backend: one file per key under a directory (`[server]
/// state_dir`), `<dir>/<escaped key>.state`.
///
/// The key is escaped with every byte outside `[A-Za-z0-9_-]`
/// percent-encoded (`.` is kept except as the first byte), so a key can
/// never name a path outside the directory or a hidden file, and two keys
/// never share a file. A save streams into a temporary file in the same
/// directory, syncs it and renames it over the snapshot, then syncs the
/// directory; a missing directory is created on the first save.
#[derive(Debug, Clone)]
pub struct FileStateStore {
    dir: PathBuf,
}

/// [`FileStateStore`] file-name extension.
const FILE_EXTENSION: &str = "state";

impl FileStateStore {
    /// A store over `dir`. No I/O: see [`Self::prepare`].
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FileStateStore { dir: dir.into() }
    }

    /// Where the snapshot of `key` lives.
    pub fn path(&self, key: &str) -> PathBuf {
        self.dir.join(file_name(key))
    }

    /// Boot-time setup: create the directory and remove temporary files
    /// older than `stale_temp_age` that a crash mid-save left behind
    /// (`.<name>.<pid>-<seq>.tmp`). A live save is never that old, so this
    /// cannot race one. Returns how many temp files were removed; an error
    /// means the directory could not be created (saves will fail too).
    pub fn prepare(&self, stale_temp_age: Duration) -> io::Result<usize> {
        fs::create_dir_all(&self.dir)?;
        Ok(self.sweep_stale_temps(stale_temp_age))
    }

    fn sweep_stale_temps(&self, max_age: Duration) -> usize {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        let now = SystemTime::now();
        let mut removed = 0;
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let is_temp = file_name.to_str().is_some_and(is_temp_name);
            let stale = || {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .is_some_and(|age| age >= max_age)
            };
            if is_temp && stale() && fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Unique per save, so two writers of one key never share a temp file.
    fn temp_path(&self, key: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        self.dir.join(format!(
            ".{}.{}-{seq}.tmp",
            file_name(key),
            std::process::id()
        ))
    }
}

impl StateStore for FileStateStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, StateError> {
        match fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, key: &str, bytes: &[u8]) -> Result<(), StateError> {
        fs::create_dir_all(&self.dir)?;
        let tmp = self.temp_path(key);
        let result = (|| {
            let mut file = File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, self.path(key))
        })();
        match result {
            Ok(()) => {
                sync_dir(&self.dir);
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.into())
            }
        }
    }

    fn describe(&self, key: &str) -> String {
        self.path(key).display().to_string()
    }
}

/// `<escaped key>.state`.
fn file_name(key: &str) -> String {
    format!("{}.{FILE_EXTENSION}", escape_key(key))
}

/// `.<anything>.state.<pid>-<seq>.tmp`: a [`FileStateStore`] temp file.
fn is_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".tmp")) else {
        return false;
    };
    let Some((stem, unique)) = rest.rsplit_once('.') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    stem.ends_with(&format!(".{FILE_EXTENSION}"))
        && unique
            .split_once('-')
            .is_some_and(|(pid, seq)| digits(pid) && digits(seq))
}

/// Make the rename durable (best effort; a no-op off Unix).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// A store key as a file-name stem (see [`FileStateStore`]).
pub fn escape_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (i, b) in key.bytes().enumerate() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (b == b'.' && i > 0) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The timing of a [`StateWriter`].
#[derive(Debug, Clone, Copy)]
pub struct WritePolicy {
    /// At most one write attempt per this interval (a forced write — the
    /// shutdown flush — skips it). A failed write is retried after it too.
    pub min_interval: Duration,
    /// An unchanged state is rewritten once the last write is this old, so
    /// the snapshot's own timestamp tells a restore how long the server was
    /// down (at most this much too long). Not shorter than `min_interval`.
    pub refresh_interval: Duration,
    /// Failures are reported (`warn: true`) at most once per this interval.
    pub warn_interval: Duration,
}

/// What [`StateWriter::write_if_due`] did.
#[derive(Debug)]
pub enum WriteOutcome {
    /// Nothing changed since the last write and it is recent, or the last
    /// attempt was less than the minimum interval ago.
    NotDue,
    Written {
        bytes: u64,
    },
    /// The write failed and the previous snapshot stays in place. `warn` is
    /// true at most once per warn interval: log a WARN then, quieter
    /// otherwise. The next attempt waits for the minimum interval too.
    Failed {
        error: StateError,
        warn: bool,
    },
}

/// The write policy for one key of a [`StateStore`] (see [`WritePolicy`]):
/// write when the engine's state revision moved since the last successful
/// write, or when that write is older than the refresh interval; at most
/// once per minimum interval; always when forced (the shutdown flush, which
/// also stamps the time the server went down). Times are passed in so the
/// policy is testable.
#[derive(Debug)]
pub struct StateWriter {
    store: Arc<dyn StateStore>,
    key: String,
    policy: WritePolicy,
    last_attempt: Option<Instant>,
    last_written: Option<Instant>,
    last_warn: Option<Instant>,
    written: Option<u64>,
}

impl StateWriter {
    pub fn new(store: Arc<dyn StateStore>, key: String, policy: WritePolicy) -> Self {
        StateWriter {
            store,
            key,
            policy,
            last_attempt: None,
            last_written: None,
            last_warn: None,
            written: None,
        }
    }

    /// Where the snapshot lives, for log lines.
    pub fn describe(&self) -> String {
        self.store.describe(&self.key)
    }

    /// The stored snapshot, `Ok(None)` when there is none yet.
    pub fn load(&self) -> Result<Option<Vec<u8>>, StateError> {
        self.store.load(&self.key)
    }

    /// The store already holds the state at `revision`, written `age` ago
    /// (it was just restored from it): no write until the state moves on or
    /// the refresh interval runs out.
    pub fn mark_restored(&mut self, revision: u64, age: Duration, now: Instant) {
        self.written = Some(revision);
        // An age beyond what `Instant` can represent is simply "long ago".
        self.last_written = now.checked_sub(age);
    }

    pub fn is_due(&self, revision: u64, now: Instant, force: bool) -> bool {
        let since = |t: Option<Instant>, d: Duration| {
            t.is_none_or(|t| now.saturating_duration_since(t) >= d)
        };
        force
            || (since(self.last_attempt, self.policy.min_interval)
                && (self.written != Some(revision)
                    || since(self.last_written, self.policy.refresh_interval)))
    }

    /// Write the state at `revision` when [`Self::is_due`]. `encode` is not
    /// called otherwise, so it may do the (costly) export.
    pub fn write_if_due(
        &mut self,
        revision: u64,
        now: Instant,
        force: bool,
        encode: impl FnOnce() -> Result<Vec<u8>, StateError>,
    ) -> WriteOutcome {
        if !self.is_due(revision, now, force) {
            return WriteOutcome::NotDue;
        }
        self.last_attempt = Some(now);
        let result = encode().and_then(|bytes| {
            self.store.save(&self.key, &bytes)?;
            Ok(bytes.len() as u64)
        });
        match result {
            Ok(bytes) => {
                self.written = Some(revision);
                self.last_written = Some(now);
                WriteOutcome::Written { bytes }
            }
            Err(error) => {
                let warn = self
                    .last_warn
                    .is_none_or(|t| now.saturating_duration_since(t) >= self.policy.warn_interval);
                if warn {
                    self.last_warn = Some(now);
                }
                WriteOutcome::Failed { error, warn }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_key_escapes_everything_that_could_leave_the_directory() {
        assert_eq!(
            escape_key("cap-meteoalarm_wis2.cap"),
            "cap-meteoalarm_wis2.cap"
        );
        assert_eq!(escape_key("radar.v2"), "radar.v2");
        assert_eq!(escape_key("../etc/passwd"), "%2E.%2Fetc%2Fpasswd");
        assert_eq!(escape_key(".hidden"), "%2Ehidden");
        assert_eq!(escape_key("a b%"), "a%20b%25");
        assert_eq!(escape_key("ä"), "%C3%A4");
        // Injective: the escape character itself is escaped.
        assert_ne!(escape_key("a/b"), escape_key("a%2Fb"));
    }

    #[test]
    fn collection_keys_and_file_paths() {
        assert_eq!(collection_key("cap-wis2", "cap"), "cap-wis2.cap");
        assert_eq!(collection_key("obs", "bufr"), "obs.bufr");
        let store = FileStateStore::new("/var/lib/mc");
        assert_eq!(
            store.path(&collection_key("x/y", "cap")),
            Path::new("/var/lib/mc/x%2Fy.cap.state")
        );
        assert_eq!(store.describe("c.cap"), "/var/lib/mc/c.cap.state");
    }

    #[test]
    fn file_save_round_trips_creates_the_dir_and_leaves_no_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested/state");
        let store = FileStateStore::new(&dir);
        assert!(store.load("c.cap").unwrap().is_none());
        store.save("c.cap", b"one").unwrap();
        assert_eq!(store.load("c.cap").unwrap().unwrap(), b"one");
        store.save("c.cap", b"two!").unwrap();
        assert_eq!(store.load("c.cap").unwrap().unwrap(), b"two!");
        // Keys are independent.
        store.save("c.bufr", b"b").unwrap();
        assert_eq!(store.load("c.cap").unwrap().unwrap(), b"two!");
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["c.bufr.state", "c.cap.state"]);
    }

    #[test]
    fn failed_save_keeps_the_previous_snapshot_and_leaves_no_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStateStore::new(tmp.path());
        store.save("c.cap", b"good").unwrap();
        // The rename fails: a non-empty directory sits where `d`'s file goes.
        let target = store.path("d.cap");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("x"), b"").unwrap();
        assert!(matches!(
            store.save("d.cap", b"new"),
            Err(StateError::Io(_))
        ));
        assert_eq!(store.load("c.cap").unwrap().unwrap(), b"good");
        let mut names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["c.cap.state", "d.cap.state"], "temp left");
    }

    #[test]
    fn unwritable_directory_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        // A regular file where the directory should be.
        let blocker = tmp.path().join("state");
        fs::write(&blocker, b"").unwrap();
        let store = FileStateStore::new(&blocker);
        assert!(matches!(store.save("c.cap", b"x"), Err(StateError::Io(_))));
        assert!(store.load("c.cap").is_err());
        assert!(store.prepare(Duration::ZERO).is_err());
    }

    #[test]
    fn prepare_creates_the_dir_and_removes_only_stale_temps() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let store = FileStateStore::new(&dir);
        assert_eq!(store.prepare(Duration::ZERO).unwrap(), 0);
        assert!(dir.is_dir());
        for name in [
            ".c.cap.state.1-0.tmp",
            ".d.bufr.state.22-7.tmp",
            ".c.cap.state.x-0.tmp",
            ".c.cap.1-0.tmp",
            "c.cap.state",
            "notes.tmp",
        ] {
            fs::write(dir.join(name), b"").unwrap();
        }
        assert_eq!(
            store.prepare(Duration::from_secs(3600)).unwrap(),
            0,
            "fresh"
        );
        assert_eq!(store.prepare(Duration::ZERO).unwrap(), 2);
        let mut left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                ".c.cap.1-0.tmp",
                ".c.cap.state.x-0.tmp",
                "c.cap.state",
                "notes.tmp"
            ]
        );
    }

    /// A backend whose saves fail on demand; records what was saved.
    #[derive(Debug, Default)]
    struct MemoryStore {
        saved: std::sync::Mutex<Vec<Vec<u8>>>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl StateStore for MemoryStore {
        fn load(&self, _: &str) -> Result<Option<Vec<u8>>, StateError> {
            Ok(self.saved.lock().unwrap().last().cloned())
        }
        fn save(&self, _: &str, bytes: &[u8]) -> Result<(), StateError> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(StateError::Backend("disk full".into()));
            }
            self.saved.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn describe(&self, key: &str) -> String {
            format!("memory:{key}")
        }
    }

    const MINUTE: Duration = Duration::from_secs(60);

    fn writer(store: &Arc<MemoryStore>) -> StateWriter {
        let store: Arc<dyn StateStore> = store.clone();
        StateWriter::new(
            store,
            "c.cap".into(),
            WritePolicy {
                min_interval: 5 * MINUTE,
                refresh_interval: 60 * MINUTE,
                warn_interval: 15 * MINUTE,
            },
        )
    }

    fn ok() -> Result<Vec<u8>, StateError> {
        Ok(b"s".to_vec())
    }

    #[test]
    fn writer_writes_changed_state_at_a_bounded_rate() {
        let store = Arc::new(MemoryStore::default());
        let mut w = writer(&store);
        assert_eq!(w.describe(), "memory:c.cap");
        let t0 = Instant::now();
        assert!(matches!(
            w.write_if_due(1, t0, false, ok),
            WriteOutcome::Written { bytes: 1 }
        ));
        // Same revision: not rewritten before the refresh interval...
        assert!(matches!(
            w.write_if_due(1, t0 + 30 * MINUTE, false, ok),
            WriteOutcome::NotDue
        ));
        // ...but a forced write (the shutdown flush) always writes.
        assert!(matches!(
            w.write_if_due(1, t0 + 30 * MINUTE, true, ok),
            WriteOutcome::Written { .. }
        ));
        // A new revision waits for the minimum interval, unless forced.
        assert!(matches!(
            w.write_if_due(2, t0 + 33 * MINUTE, false, ok),
            WriteOutcome::NotDue
        ));
        assert!(w.is_due(2, t0 + 35 * MINUTE, false));
        assert!(matches!(
            w.write_if_due(2, t0 + 33 * MINUTE, true, ok),
            WriteOutcome::Written { .. }
        ));
        assert_eq!(store.saved.lock().unwrap().len(), 3);
        // An unchanged state is rewritten once the last write is a refresh
        // interval old: the snapshot's timestamp tracks the server's life.
        assert!(!w.is_due(2, t0 + 92 * MINUTE, false));
        assert!(matches!(
            w.write_if_due(2, t0 + 93 * MINUTE, false, ok),
            WriteOutcome::Written { .. }
        ));
    }

    #[test]
    fn a_restored_snapshot_counts_as_written_when_it_was() {
        let store = Arc::new(MemoryStore::default());
        let t0 = Instant::now() + 24 * 60 * MINUTE;
        // Restored from a snapshot written 50 minutes ago: nothing to write
        // until it changes or turns an hour old.
        let mut w = writer(&store);
        w.mark_restored(7, 50 * MINUTE, t0);
        assert!(!w.is_due(7, t0, false));
        assert!(!w.is_due(7, t0 + 9 * MINUTE, false));
        assert!(w.is_due(7, t0 + 10 * MINUTE, false));
        assert!(w.is_due(8, t0, false));
        // An age `Instant` cannot represent is simply long ago.
        let mut w = writer(&store);
        w.mark_restored(7, Duration::MAX, t0);
        assert!(w.is_due(7, t0, false));
    }

    #[test]
    fn writer_rate_limits_failure_warnings() {
        let store = Arc::new(MemoryStore::default());
        let mut w = writer(&store);
        let t1 = Instant::now();
        store.fail.store(true, Ordering::Relaxed);
        let warned = |o: WriteOutcome| match o {
            WriteOutcome::Failed { warn, .. } => warn,
            other => panic!("expected a failure, got {other:?}"),
        };
        // The first failure warns, the retries every minimum interval do
        // not, the one after the warn interval warns again.
        assert!(warned(w.write_if_due(4, t1, false, ok)));
        assert!(matches!(
            w.write_if_due(4, t1 + 4 * MINUTE, false, ok),
            WriteOutcome::NotDue
        ));
        assert!(!warned(w.write_if_due(4, t1 + 5 * MINUTE, false, ok)));
        assert!(!warned(w.write_if_due(4, t1 + 10 * MINUTE, false, ok)));
        assert!(warned(w.write_if_due(4, t1 + 15 * MINUTE, false, ok)));
        // The failed revision is still pending.
        assert!(w.is_due(4, t1 + 20 * MINUTE, false));
        // An encoding failure never reaches the store.
        store.fail.store(false, Ordering::Relaxed);
        let bad = || Err(StateError::Encode("nan".into()));
        match w.write_if_due(4, t1 + 20 * MINUTE, false, bad) {
            WriteOutcome::Failed { error, .. } => {
                assert_eq!(error.to_string(), "snapshot encoding failed: nan")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(store.saved.lock().unwrap().is_empty());
        assert!(matches!(
            w.write_if_due(4, t1 + 25 * MINUTE, false, ok),
            WriteOutcome::Written { .. }
        ));
    }
}
