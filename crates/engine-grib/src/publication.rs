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
    /// The newest run prefix's index files as listed, by path, and the
    /// files read before: a path seen again with the same version is not
    /// new. Pruned to the files read and those the last listing had.
    versions: HashMap<String, Version>,
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
    /// `read(path)`: whether the index at `path` has been read;
    /// `in_newest(path)`: whether it was read into the newest run. A new or
    /// rewritten file counts unless it was read into an older run; one not
    /// read yet (a read that failed or was deferred) counts when first seen.
    pub(crate) fn observe(
        &mut self,
        now: DateTime<Utc>,
        newest: Option<DateTime<Utc>>,
        listed: Option<&HashMap<String, Version>>,
        read: impl Fn(&str) -> bool,
        in_newest: impl Fn(&str) -> bool,
    ) -> bool {
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
                self.versions.get(path) != Some(version) && (in_newest(path) || !read(path))
            });
            if changed && !self.finished && self.run.is_some() {
                self.seen = Some(now);
            }
            self.versions.extend(
                listed
                    .iter()
                    .map(|(path, version)| (path.clone(), *version)),
            );
        }
        self.versions
            .retain(|path, _| listed.is_some_and(|listed| listed.contains_key(path)) || read(path));
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
        assert!(publication.observe(t(0), run, Some(&listed), |_| true, |_| true));
        assert!(publication.observe(t(29), run, Some(&listed), |_| true, |_| true));
        assert!(!publication.observe(t(30), run, Some(&listed), |_| true, |_| true));
    }

    /// A finished run stays finished; a newer run publishes again.
    #[test]
    fn a_finished_run_is_not_reopened() {
        let mut publication = Publication::default();
        let run = Some(t(-460));
        let listed = listing(&[("a", 1, 0)]);
        assert!(publication.observe(t(0), run, Some(&listed), |_| true, |_| true));
        assert!(!publication.observe(t(30), run, Some(&listed), |_| true, |_| true));
        let rewritten = listing(&[("a", 2, 31), ("b", 1, 31)]);
        assert!(!publication.observe(t(31), run, Some(&rewritten), |_| true, |_| true));
        assert!(!publication.publishing(t(32)));
        let newer = Some(t(-100));
        assert!(publication.observe(t(40), newer, Some(&rewritten), |_| true, |_| false));
    }

    /// A file read into an older run is not the newest run publishing; one
    /// not read yet is, when first seen only.
    #[test]
    fn only_the_newest_runs_files_count() {
        let mut publication = Publication::default();
        let run = Some(t(-460));
        assert!(publication.observe(t(0), run, None, |_| true, |_| true));
        let old = listing(&[("old", 1, 10)]);
        assert!(publication.observe(t(10), run, Some(&old), |_| true, |_| false));
        assert!(!publication.observe(t(30), run, Some(&old), |_| true, |_| false));

        let mut publication = Publication::default();
        assert!(publication.observe(t(0), run, None, |_| true, |_| true));
        let unread = listing(&[("unread", 1, 10)]);
        assert!(publication.observe(t(10), run, Some(&unread), |_| false, |_| false));
        assert!(publication.observe(t(39), run, Some(&unread), |_| false, |_| false));
        assert!(!publication.observe(t(40), run, Some(&unread), |_| false, |_| false));
    }
}
