//! When a file seen being written is complete enough to catalogue.
//!
//! A poll can see a file its producer is still writing. Catalogued then, a
//! reader opens a truncated file. So a newly seen file waits as a
//! [`PendingFile`] until polls see it unchanged: engine-geotiff's local scan
//! and the [`TemplateProber`](super::TemplateProber) both settle files this
//! way.

use crate::ObjectVersion;

/// How many polls after the first must see a pending file's version
/// unchanged before it settles. A file first seen on poll N settles on poll
/// N + 2 at the earliest.
pub const STABLE_POLLS: u8 = 2;

/// A file seen but not settled: the version the last poll saw and how many
/// consecutive polls since the first have seen that same version.
///
/// A caller that knows only a file's size (engine-geotiff's local scan)
/// tracks [`ObjectVersion::sized`]; a remote one tracks the size and
/// `Last-Modified` a HEAD reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFile {
    version: ObjectVersion,
    stable_polls: u8,
}

impl PendingFile {
    /// A file first seen as `version`.
    pub fn new(version: ObjectVersion) -> Self {
        Self {
            version,
            stable_polls: 0,
        }
    }

    /// Record the version this poll saw. Another version restarts the
    /// count from it. Returns whether the file has now settled: seen as the
    /// same version on [`STABLE_POLLS`] consecutive polls after the first.
    pub fn observe(&mut self, version: ObjectVersion) -> bool {
        if version == self.version {
            self.stable_polls = self.stable_polls.saturating_add(1);
        } else {
            *self = Self::new(version);
        }
        self.stable_polls >= STABLE_POLLS
    }

    /// The version the last poll saw.
    pub fn version(&self) -> &ObjectVersion {
        &self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(size: u64, minute: u32) -> ObjectVersion {
        ObjectVersion {
            size,
            last_modified: Some(
                chrono::NaiveDate::from_ymd_opt(2026, 10, 17)
                    .unwrap()
                    .and_hms_opt(8, minute, 0)
                    .unwrap()
                    .and_utc(),
            ),
            e_tag: None,
        }
    }

    /// Settled on the second poll after the first that sees the same size
    /// and `Last-Modified`; a change of either restarts the count.
    #[test]
    fn settles_after_two_unchanged_polls_and_restarts_on_a_change() {
        let mut pending = PendingFile::new(version(100, 0));
        assert!(!pending.observe(version(100, 0)));
        assert!(pending.observe(version(100, 0)));

        let mut pending = PendingFile::new(version(100, 0));
        assert!(!pending.observe(version(100, 0)));
        assert!(!pending.observe(version(150, 0)), "grew: still written");
        assert!(!pending.observe(version(150, 0)));
        assert!(!pending.observe(version(150, 1)), "same size, rewritten");
        assert!(!pending.observe(version(150, 1)));
        assert!(pending.observe(version(150, 1)));
        assert_eq!(pending.version(), &version(150, 1));
    }

    /// A size-only version (engine-geotiff) settles on its size alone.
    #[test]
    fn a_sized_version_settles_on_its_size() {
        let mut pending = PendingFile::new(ObjectVersion::sized(10));
        assert!(!pending.observe(ObjectVersion::sized(10)));
        assert!(!pending.observe(ObjectVersion::sized(12)));
        assert!(!pending.observe(ObjectVersion::sized(12)));
        assert!(pending.observe(ObjectVersion::sized(12)));
        assert!(pending.observe(ObjectVersion::sized(12)), "stays settled");
    }
}
