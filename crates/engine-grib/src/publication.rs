//! Whether the newest run is still publishing (#1562), which keeps it on
//! top of `max_runs`, judged by the engine's own clock.
//!
//! A run publishes from when the catalog first holds it until no scan has
//! seen a new or rewritten index file of it for [`PUBLISHING_GRACE`]. The
//! store's modification times only tell a rewritten file from its earlier
//! listing; they are never compared with the clock, so a store whose clock
//! runs ahead or behind changes nothing. Once judged finished, a run stays
//! finished: a later rewrite, or a stalled run resuming, does not keep an
//! extra run again, nor have a full re-list read back a run already
//! evicted. Only a newer run publishes again.
//!
//! An index a scan left unread because its source backed off (#1530) is
//! still to come, not unreadable: while a scan lists one, the run is still
//! publishing, however long the source throttles, until it is read. An
//! index that cannot be read (not UTF-8, rejected by the parser) counts
//! once, when first listed.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

/// How long after a scan last saw a new or rewritten index of the newest
/// run that run counts as still publishing: the 30 minutes the template
/// prober gives a run's missing files
/// ([`ds_storage::discovery::DEFAULT_RUN_GRACE`]). A run whose publication
/// stalls is finished after it.
pub(crate) const PUBLISHING_GRACE: chrono::Duration = ds_storage::discovery::DEFAULT_RUN_GRACE;

/// An index file as listed: its size and last-modified time, compared only
/// with its own earlier listing.
pub(crate) type Version = (u64, DateTime<Utc>);

/// How long a file not read is remembered after it was last listed: the
/// two days of run prefixes a scan lists. A file that cannot be read (not
/// UTF-8, an index the parser rejects, a GET that keeps failing) is then
/// new once, not again after every listing that misses it.
const UNREAD_MEMORY: chrono::Duration = chrono::Duration::days(2);

/// What a scan knows of the index files it listed.
pub(crate) struct Reads<R, D, N> {
    /// Whether the index at a path has been read.
    pub read: R,
    /// Whether it was left unread because the source backed off.
    pub deferred: D,
    /// Whether it was read into the newest run.
    pub in_newest: N,
}

/// What the scans have seen of the newest run's publication.
#[derive(Debug, Default)]
pub(crate) struct Publication {
    /// The catalog's newest run; `None` before one.
    run: Option<DateTime<Utc>>,
    /// When, by the engine's clock, a scan last saw a new or rewritten index
    /// of it, or first catalogued it.
    seen: Option<DateTime<Utc>>,
    /// Judged finished; only a newer run publishes again.
    finished: bool,
    /// The newest run prefix's index files as listed, by path, with when a
    /// scan last listed each: a path seen again with the same version is
    /// not new, read or not. Pruned to the files read and those listed
    /// within [`UNREAD_MEMORY`], so a scan that lists nothing, or another
    /// prefix, forgets none it will list again.
    versions: HashMap<String, (Version, DateTime<Utc>)>,
}

impl Publication {
    /// Whether the newest run publishes at `now`, as the last scan left it.
    pub(crate) fn publishing(&self, now: DateTime<Utc>) -> bool {
        !self.finished && self.seen.is_some_and(|seen| now - seen < PUBLISHING_GRACE)
    }

    /// Record a scan at `now` of a catalog whose newest run is `newest`, and
    /// return whether that run publishes. `listed` holds the index files the
    /// scan listed in the newest run prefix with files (the source's one
    /// prefix for a fixed-prefix source), `None` when it listed none.
    /// `reads` says which were read, left unread by a back-off, and read
    /// into the newest run ([`Reads`]). A new or rewritten file counts
    /// unless it was read into an older run; one not read yet (a read that
    /// failed) counts when first seen; one a back-off left unread counts at
    /// every listing until it is read.
    pub(crate) fn observe(
        &mut self,
        now: DateTime<Utc>,
        newest: Option<DateTime<Utc>>,
        listed: Option<&HashMap<String, Version>>,
        reads: Reads<impl Fn(&str) -> bool, impl Fn(&str) -> bool, impl Fn(&str) -> bool>,
    ) -> bool {
        let Reads {
            read,
            deferred,
            in_newest,
        } = reads;
        if newest > self.run {
            // A newer run (or the first since a restart).
            self.seen = Some(now);
            self.finished = false;
        } else if newest < self.run {
            // The newest run left the catalog (retention): what is left
            // has finished.
            self.finished = true;
        }
        self.run = newest;
        if let Some(listed) = listed {
            let changed = listed.iter().any(|(path, version)| {
                let unread = !read(path);
                (unread && deferred(path))
                    || (self.versions.get(path).map(|(seen, _)| seen) != Some(version)
                        && (in_newest(path) || unread))
            });
            if changed && !self.finished && self.run.is_some() {
                self.seen = Some(now);
            }
            self.versions.extend(
                listed
                    .iter()
                    .map(|(path, version)| (path.clone(), (*version, now))),
            );
        }
        self.versions
            .retain(|path, (_, listed)| now - *listed < UNREAD_MEMORY || read(path));
        let publishing = self.publishing(now);
        if !publishing && self.run.is_some() {
            self.finished = true;
        }
        publishing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a scan knows of its listed files: `read` and `in_newest` for
    /// every one, none left unread by a back-off.
    fn reads(
        read: impl Fn(&str) -> bool,
        in_newest: impl Fn(&str) -> bool,
    ) -> Reads<impl Fn(&str) -> bool, impl Fn(&str) -> bool, impl Fn(&str) -> bool> {
        Reads {
            read,
            deferred: |_: &str| false,
            in_newest,
        }
    }

    fn t(minutes: i64) -> DateTime<Utc> {
        "2026-04-06T07:40:00Z".parse::<DateTime<Utc>>().unwrap()
            + chrono::Duration::minutes(minutes)
    }

    fn listing(files: &[(&str, u64, i64)]) -> HashMap<String, Version> {
        files
            .iter()
            .map(|&(path, size, written)| (path.to_string(), (size, t(written))))
            .collect()
    }

    /// The store's times, however far off, only tell a rewritten file.
    #[test]
    fn the_grace_runs_on_the_engines_clock() {
        let mut publication = Publication::default();
        let run = Some(t(-460));
        // Written "three hours from now" by a store whose clock is ahead.
        let listed = listing(&[("a", 1, 180)]);
        assert!(publication.observe(t(0), run, Some(&listed), reads(|_| true, |_| true)));
        assert!(publication.observe(t(29), run, Some(&listed), reads(|_| true, |_| true)));
        assert!(!publication.observe(t(30), run, Some(&listed), reads(|_| true, |_| true)));
    }

    /// A finished run stays finished; a newer run publishes again.
    #[test]
    fn a_finished_run_is_not_reopened() {
        let mut publication = Publication::default();
        let run = Some(t(-460));
        let listed = listing(&[("a", 1, 0)]);
        assert!(publication.observe(t(0), run, Some(&listed), reads(|_| true, |_| true)));
        assert!(!publication.observe(t(30), run, Some(&listed), reads(|_| true, |_| true)));
        let rewritten = listing(&[("a", 2, 31), ("b", 1, 31)]);
        assert!(!publication.observe(t(31), run, Some(&rewritten), reads(|_| true, |_| true)));
        assert!(!publication.publishing(t(32)));
        let newer = Some(t(-100));
        assert!(publication.observe(t(40), newer, Some(&rewritten), reads(|_| true, |_| false)));
    }

    /// A file read into an older run is not the newest run publishing; one
    /// not read yet is, when first seen only.
    #[test]
    fn only_the_newest_runs_files_count() {
        let mut publication = Publication::default();
        let run = Some(t(-460));
        assert!(publication.observe(t(0), run, None, reads(|_| true, |_| true)));
        let old = listing(&[("old", 1, 10)]);
        assert!(publication.observe(t(10), run, Some(&old), reads(|_| true, |_| false)));
        assert!(!publication.observe(t(30), run, Some(&old), reads(|_| true, |_| false)));

        let mut publication = Publication::default();
        assert!(publication.observe(t(0), run, None, reads(|_| true, |_| true)));
        let unread = listing(&[("unread", 1, 10)]);
        // A scan that lists nothing between does not make it new again.
        assert!(publication.observe(t(10), run, Some(&unread), reads(|_| false, |_| false)));
        assert!(publication.observe(t(15), run, None, reads(|_| false, |_| false)));
        assert!(publication.observe(t(20), run, Some(&unread), reads(|_| false, |_| false)));
        assert!(!publication.observe(t(40), run, Some(&unread), reads(|_| false, |_| false)));

        let mut publication = Publication::default();
        assert!(publication.observe(t(0), run, None, reads(|_| true, |_| true)));
        assert!(publication.observe(t(10), run, Some(&unread), reads(|_| false, |_| false)));
        assert!(publication.observe(t(39), run, Some(&unread), reads(|_| false, |_| false)));
        assert!(!publication.observe(t(40), run, Some(&unread), reads(|_| false, |_| false)));
    }

    /// An index a back-off left unread keeps the run publishing past the
    /// grace while scans list it, unlike one that cannot be read; once read,
    /// the grace runs from the last scan that listed it unread.
    #[test]
    fn an_index_a_back_off_left_unread_keeps_the_run_publishing() {
        let mut publication = Publication::default();
        let run = Some(t(-460));
        let listed = listing(&[("read", 1, 0), ("deferred", 1, 0)]);
        let deferred = |read: bool| Reads {
            read: move |path: &str| path == "read" || read,
            deferred: |path: &str| path == "deferred",
            in_newest: |_: &str| true,
        };
        assert!(publication.observe(t(0), run, Some(&listed), deferred(false)));
        assert!(publication.observe(t(31), run, Some(&listed), deferred(false)));
        assert!(publication.observe(t(45), run, Some(&listed), deferred(false)));
        assert!(publication.observe(t(50), run, Some(&listed), deferred(true)));
        assert!(publication.observe(t(74), run, Some(&listed), deferred(true)));
        assert!(!publication.observe(t(75), run, Some(&listed), deferred(true)));
    }
}
