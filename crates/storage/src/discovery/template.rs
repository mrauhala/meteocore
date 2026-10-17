//! Listing-free discovery: render the keys a source's files would have and
//! HEAD them (#287, #1067).
//!
//! Some HTTP servers cannot be listed: a plain autoindex answers no WebDAV
//! `PROPFIND`, and thredds.met.no's fileServer answers it with 400. A
//! [`TemplateProber`] instead renders candidate keys from a prefix template
//! and a filename template for the times files could hold, and HEADs them,
//! at most [`PROBE_CONCURRENCY`] at a time on one storage-bridge call
//! ([`DataStore::head_many`], Critical Rule 9). Call [`TemplateProber::poll`]
//! where [`DataStore::head`] may be called: from the background poll
//! runtime, never from a request handler.
//!
//! # Slots, leads and the sentinel
//!
//! The candidate times are slots. [`Schedule::Cadence`] has one per cadence
//! step, walked back from now (a radar composite every 5 minutes,
//! engine-odim). [`Schedule::Runs`] has one per model run, at each run hour
//! of each day (MEPS every 3 hours). A slot holds one file per configured
//! lead ([`TemplateSpec::leads`]), or one file when there are no leads.
//!
//! The first lead is the slot's sentinel. While a slot's sentinel has not
//! been seen, the slot costs one HEAD per poll, the sentinel's. Its other
//! leads are HEADed once the sentinel exists, so a MEPS run of 67 files
//! costs 1 HEAD per poll, not 67, until it appears.
//!
//! # Settling and publication
//!
//! Each file goes from absent to seen-unsettled to settled ([`Settling`]).
//! A seen-unsettled file is re-HEADed on every poll until it settles.
//!
//! A slot is published all at once: when every lead has settled, or
//! [`TemplateSpec::run_grace`] after its sentinel was first seen, with the
//! leads settled by then and a WARN listing the missing ones (a run with 39
//! of 67 leads still works). [`TemplateProber::files`] lists the settled
//! files of published slots only, and every poll's [`ProbeReport`] says
//! which files joined and left that list.
//!
//! # Replaced files
//!
//! A settled file is replaced when a later HEAD reports another
//! [`ObjectVersion`] (size, `Last-Modified`, ETag). Unless the spec is
//! [`TemplateSpec::immutable`], every [`SETTLED_RECHECK`] each published
//! slot's sentinel is re-HEADed, together with its leads still missing (a
//! late upload). A sentinel found changed or gone has every settled file of
//! its slot re-HEADed on the next poll. A changed file is withdrawn and
//! settles again as its new version; a vanished one is withdrawn.
//!
//! # The per-poll cap
//!
//! One poll makes at most [`TemplateSpec::max_heads_per_poll`] HEADs, in
//! this order: unsettled files; the files of slots whose sentinel changed;
//! newest slot first, the sentinels and missing leads of unpublished slots;
//! then the due rechecks. What the cap leaves waits for the next poll
//! ([`ProbeReport::deferred`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Duration, NaiveTime, Timelike, Utc};
use ds_core::error::DataServerError;
use object_store::path::Path as ObjectPath;

use super::settle::PendingFile;
use super::{
    prefix_step, validate_run_prefix_pattern, PrefixStep, TimeWindow, LEAD_DIGITS,
    LEAD_PLACEHOLDER, RUN_PLACEHOLDER,
};
use crate::{DataStore, ObjectVersion};

/// Most slots one prober tracks: 24 h of a 5-minute cadence. A backstop
/// for a cadence without a `time_window`; `max_slots` and the window clamp
/// it lower.
pub const MAX_TEMPLATE_SLOTS: usize = 288;

/// Most HEADs in flight at once.
pub const PROBE_CONCURRENCY: usize = 16;

/// [`TemplateSpec::max_heads_per_poll`] unless set.
pub const DEFAULT_MAX_HEADS_PER_POLL: usize = 288;

/// How often a published slot's sentinel is re-HEADed to catch a replaced
/// file, unless the spec is [`TemplateSpec::immutable`].
pub const SETTLED_RECHECK: Duration = Duration::minutes(30);

/// [`TemplateSpec::run_grace`] unless set.
pub const DEFAULT_RUN_GRACE: Duration = Duration::minutes(30);

/// [`Settling::Stable`]'s `min_age` in [`TemplateSpec::new`].
pub const DEFAULT_MIN_AGE: Duration = Duration::minutes(10);

/// Which times a template source's files are probed for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// One slot per `cadence` step, aligned to the Unix epoch and walked
    /// back from now. The filename template names the slot time with
    /// strftime codes, `%H%M` included.
    Cadence(Duration),
    /// One slot per model run: each day at each of `run_hours` (0-23).
    /// The slot time is the run's reference time. The keys name its date
    /// with strftime date codes and its hour with [`RUN_PLACEHOLDER`];
    /// `%H` is rejected, as in [`validate_run_prefix_pattern`].
    Runs { run_hours: Vec<u32> },
}

/// When a seen file is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settling {
    /// On the poll that first sees it: files that are renamed into place
    /// complete, such as engine-odim's radar composites.
    OnSight,
    /// Once [`super::STABLE_POLLS`] polls after the first see the same
    /// version ([`PendingFile`]), or at once when its `Last-Modified` is
    /// `min_age` old. A file without `Last-Modified` settles on polls only.
    Stable { min_age: Duration },
}

/// Where a template source's files are, and when they are published.
/// [`TemplateSpec::new`] sets the defaults; set the rest with
/// struct-update syntax.
#[derive(Debug, Clone)]
pub struct TemplateSpec {
    /// Literal key prefix every key starts with, such as an HTTP source's
    /// URL path. Never expanded.
    pub base: String,
    /// strftime prefix under `base`, expanded with the slot time: date and
    /// hour codes, and [`RUN_PLACEHOLDER`] for [`Schedule::Runs`]. May be
    /// empty.
    pub prefix_pattern: String,
    /// strftime filename, expanded with the slot time, with
    /// [`RUN_PLACEHOLDER`] for [`Schedule::Runs`] and [`LEAD_PLACEHOLDER`]
    /// when there are `leads`.
    pub filename_template: String,
    pub schedule: Schedule,
    /// The leads each slot holds, the first being the sentinel; each below
    /// 10^[`LEAD_DIGITS`]. Empty when the filename has no
    /// [`LEAD_PLACEHOLDER`].
    pub leads: Vec<u32>,
    /// How far back from now slots are probed; its sign is ignored.
    /// Required for [`Schedule::Runs`]. A cadence without one probes
    /// [`MAX_TEMPLATE_SLOTS`] steps.
    pub time_window: Option<TimeWindow>,
    /// At most this many of the newest slots (engine-odim's `max_files`).
    pub max_slots: Option<usize>,
    /// HEAD requests one poll may make, at least 1.
    pub max_heads_per_poll: usize,
    pub settling: Settling,
    /// Never re-HEAD a settled file: published files cannot be replaced.
    pub immutable: bool,
    /// How long after its sentinel was first seen a slot with missing
    /// leads is published without them.
    pub run_grace: Duration,
    /// An object larger than this is treated as absent, logged at WARN.
    pub max_size: Option<u64>,
    /// Prefix of log lines, normally the collection id.
    pub label: String,
}

impl TemplateSpec {
    /// A spec without a prefix pattern, leads, window or slot limit, at
    /// [`DEFAULT_MAX_HEADS_PER_POLL`], settling [`Settling::Stable`] at
    /// [`DEFAULT_MIN_AGE`], rechecking settled files, with
    /// [`DEFAULT_RUN_GRACE`].
    pub fn new(
        base: impl Into<String>,
        filename_template: impl Into<String>,
        schedule: Schedule,
        label: impl Into<String>,
    ) -> Self {
        Self {
            base: base.into(),
            prefix_pattern: String::new(),
            filename_template: filename_template.into(),
            schedule,
            leads: Vec::new(),
            time_window: None,
            max_slots: None,
            max_heads_per_poll: DEFAULT_MAX_HEADS_PER_POLL,
            settling: Settling::Stable {
                min_age: DEFAULT_MIN_AGE,
            },
            immutable: false,
            run_grace: DEFAULT_RUN_GRACE,
            max_size: None,
            label: label.into(),
        }
    }
}

/// A settled file of a published slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedFile {
    /// The slot time: the run's reference time, or the cadence step.
    pub time: DateTime<Utc>,
    pub lead: Option<u32>,
    /// The full object key.
    pub key: String,
    pub version: ObjectVersion,
}

/// What one [`TemplateProber::poll`] did.
#[derive(Debug, Default)]
pub struct ProbeReport {
    /// HEAD requests made.
    pub heads: usize,
    /// HEADs that failed other than with a 404, each logged at WARN.
    pub failed: usize,
    /// HEADs the per-poll cap left for the next poll.
    pub deferred: usize,
    /// Slots published, whole or partial.
    pub published: Vec<DateTime<Utc>>,
    /// Files that joined [`TemplateProber::files`].
    pub added: Vec<ProbedFile>,
    /// Files that left it, as the version they were listed with: replaced,
    /// vanished, grown past `max_size`, or aged out of the window. A caller
    /// drops what it holds for each.
    pub withdrawn: Vec<ProbedFile>,
}

impl ProbeReport {
    /// Whether [`TemplateProber::files`] changed.
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || !self.withdrawn.is_empty()
    }
}

/// Probes a template source on every poll and keeps what it found: the
/// state of each candidate file of each slot in the window. See the
/// [module docs](self).
#[derive(Debug)]
pub struct TemplateProber {
    spec: TemplateSpec,
    /// The leads of a slot, the sentinel first; `[None]` without leads.
    leads: Vec<Option<u32>>,
    slots: BTreeMap<DateTime<Utc>, Slot>,
}

#[derive(Debug, Default)]
struct Slot {
    /// The files HEADed, by lead. A lead not here has not been HEADed yet.
    files: BTreeMap<Option<u32>, FileState>,
    /// When the sentinel was first seen; the grace period starts here.
    sentinel_seen: Option<DateTime<Utc>>,
    published: bool,
    /// When the published slot's sentinel was last re-HEADed.
    checked: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
enum FileState {
    /// Not found, or not answered (its HEAD failed).
    Absent,
    Unsettled(PendingFile),
    /// `recheck`: re-HEAD on the next poll, because the slot's sentinel
    /// changed.
    Settled {
        version: ObjectVersion,
        recheck: bool,
    },
}

/// Why a poll HEADs a file, in priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Why {
    Unsettled,
    Recheck,
    Discover,
    Due,
}

#[derive(Debug)]
struct Probe {
    time: DateTime<Utc>,
    lead: Option<u32>,
    key: String,
    why: Why,
}

impl TemplateProber {
    /// A prober with nothing found yet, or a config error naming what in
    /// `spec` cannot be probed.
    pub fn new(spec: TemplateSpec) -> Result<Self, DataServerError> {
        validate(&spec)?;
        let leads = if spec.leads.is_empty() {
            vec![None]
        } else {
            spec.leads.iter().copied().map(Some).collect()
        };
        Ok(Self {
            spec,
            leads,
            slots: BTreeMap::new(),
        })
    }

    pub fn spec(&self) -> &TemplateSpec {
        &self.spec
    }

    /// The key of `lead`'s file in the slot at `time`.
    pub fn key(&self, time: DateTime<Utc>, lead: Option<u32>) -> String {
        let run = matches!(self.spec.schedule, Schedule::Runs { .. });
        let render = |pattern: &str| {
            let mut pattern = pattern.to_string();
            if run {
                pattern = pattern.replace(RUN_PLACEHOLDER, &format!("{:02}", time.hour()));
            }
            if let Some(lead) = lead {
                pattern = pattern.replace(
                    LEAD_PLACEHOLDER,
                    &format!("{lead:0width$}", width = LEAD_DIGITS),
                );
            }
            let mut rendered = String::new();
            // `validate` rejected every specifier chrono cannot format.
            let _ = write!(rendered, "{}", time.format(&pattern));
            rendered
        };
        let prefix = render(&self.spec.prefix_pattern);
        let filename = render(&self.spec.filename_template);
        [
            self.spec.base.trim_matches('/'),
            prefix.trim_matches('/'),
            &filename,
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
    }

    /// The settled files of every published slot, oldest slot first, each
    /// slot's in lead order.
    pub fn files(&self) -> Vec<ProbedFile> {
        self.slots
            .iter()
            .filter(|(_, slot)| slot.published)
            .flat_map(|(&time, slot)| self.settled(time, slot))
            .collect()
    }

    /// Probe once at `now`: forget the slots that left the window, HEAD
    /// what the plan and the per-poll cap allow, settle, and publish.
    ///
    /// A HEAD that fails leaves its file as it was; it is logged at WARN
    /// and counted in [`ProbeReport::failed`]. The `Err` is a
    /// storage-bridge failure, after which nothing but the window moved.
    pub fn poll(
        &mut self,
        store: &DataStore,
        now: DateTime<Utc>,
    ) -> Result<ProbeReport, DataServerError> {
        let mut report = ProbeReport::default();
        let window: BTreeSet<DateTime<Utc>> = self.slot_times(now).into_iter().collect();
        let aged: Vec<DateTime<Utc>> = self
            .slots
            .keys()
            .filter(|time| !window.contains(time))
            .copied()
            .collect();
        for time in aged {
            if let Some(slot) = self.slots.remove(&time) {
                if slot.published {
                    report.withdrawn.extend(self.settled(time, &slot));
                }
            }
        }
        for &time in &window {
            self.slots.entry(time).or_default();
        }

        let mut plan = self.plan(now);
        report.deferred = plan.len().saturating_sub(self.spec.max_heads_per_poll);
        let cut = plan.split_off(plan.len() - report.deferred);
        if !plan.is_empty() {
            let paths: Vec<ObjectPath> = plan
                .iter()
                .map(|probe| ObjectPath::from(probe.key.as_str()))
                .collect();
            let answers = store.head_many(&paths, PROBE_CONCURRENCY)?;
            report.heads = paths.len();
            for (probe, answer) in plan.iter().zip(answers) {
                self.apply(now, probe, answer, &mut report);
            }
        }
        // A due slot counts as rechecked once none of its HEADs were cut.
        let unchecked: BTreeSet<DateTime<Utc>> = cut
            .iter()
            .filter(|probe| probe.why == Why::Due)
            .map(|probe| probe.time)
            .collect();
        for probe in plan.iter().filter(|probe| probe.why == Why::Due) {
            if !unchecked.contains(&probe.time) {
                if let Some(slot) = self.slots.get_mut(&probe.time) {
                    slot.checked = Some(now);
                }
            }
        }
        self.publish(now, &mut report);
        Ok(report)
    }

    /// The slot times in the window at `now`, newest first.
    fn slot_times(&self, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        let span = self
            .spec
            .time_window
            .as_ref()
            .map(|window| Duration::seconds(window.seconds.abs()));
        let max = self
            .spec
            .max_slots
            .unwrap_or(usize::MAX)
            .clamp(1, MAX_TEMPLATE_SLOTS);
        match &self.spec.schedule {
            Schedule::Cadence(cadence) => {
                let step = cadence.num_seconds().max(1);
                // +1 so the walk includes now's own aligned step.
                let steps = span.map_or(MAX_TEMPLATE_SLOTS, |span| {
                    usize::try_from(span.num_seconds() / step).unwrap_or(usize::MAX) + 1
                });
                let aligned = now.timestamp() - now.timestamp().rem_euclid(step);
                (0..steps.min(max) as i64)
                    .filter_map(|i| DateTime::from_timestamp(aligned - i * step, 0))
                    .collect()
            }
            Schedule::Runs { run_hours } => {
                // `validate` requires a window for runs.
                let start = now - span.unwrap_or_default();
                let mut runs = Vec::new();
                let mut day = start.date_naive();
                while day <= now.date_naive() {
                    let midnight = day.and_time(NaiveTime::MIN).and_utc();
                    runs.extend(
                        run_hours
                            .iter()
                            .map(|&hour| midnight + Duration::hours(i64::from(hour)))
                            .filter(|run| (start..=now).contains(run)),
                    );
                    let Some(next) = day.succ_opt() else { break };
                    day = next;
                }
                runs.sort_unstable_by(|a, b| b.cmp(a));
                runs.dedup();
                runs.truncate(max);
                runs
            }
        }
    }

    /// What this poll would HEAD, in priority order, before the cap.
    fn plan(&self, now: DateTime<Utc>) -> Vec<Probe> {
        let mut plan = Vec::new();
        let mut push = |time: DateTime<Utc>, lead: Option<u32>, why: Why| {
            plan.push(Probe {
                time,
                lead,
                key: self.key(time, lead),
                why,
            })
        };
        let sentinel = self.leads[0];
        for (&time, slot) in self.slots.iter().rev() {
            for (&lead, state) in &slot.files {
                match state {
                    FileState::Unsettled(_) => push(time, lead, Why::Unsettled),
                    FileState::Settled { recheck: true, .. } => push(time, lead, Why::Recheck),
                    _ => {}
                }
            }
        }
        for (&time, slot) in self.slots.iter().rev() {
            if slot.published {
                continue;
            }
            if !slot.is_seen(sentinel) {
                push(time, sentinel, Why::Discover);
                continue;
            }
            for &lead in &self.leads[1..] {
                if !slot.is_seen(lead) {
                    push(time, lead, Why::Discover);
                }
            }
        }
        if !self.spec.immutable {
            for (&time, slot) in self.slots.iter().rev() {
                let due = slot
                    .checked
                    .is_none_or(|checked| now - checked >= SETTLED_RECHECK);
                if !slot.published || !due {
                    continue;
                }
                for &lead in &self.leads {
                    match slot.files.get(&lead) {
                        Some(FileState::Settled { recheck: false, .. }) if lead == sentinel => {
                            push(time, lead, Why::Due)
                        }
                        None | Some(FileState::Absent) => push(time, lead, Why::Due),
                        _ => {}
                    }
                }
            }
        }
        plan.sort_by_key(|probe| probe.why);
        plan
    }

    /// Fold one HEAD's answer into the file's state.
    fn apply(
        &mut self,
        now: DateTime<Utc>,
        probe: &Probe,
        answer: Result<Option<object_store::ObjectMeta>, DataServerError>,
        report: &mut ProbeReport,
    ) {
        let sentinel = probe.lead == self.leads[0];
        let label = &self.spec.label;
        let Some(slot) = self.slots.get_mut(&probe.time) else {
            return;
        };
        let found = match answer {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("[{label}] HEAD `{}` failed: {e}", probe.key);
                report.failed += 1;
                // Answered for the grace period's sake; otherwise as it was.
                slot.files.entry(probe.lead).or_insert(FileState::Absent);
                return;
            }
        };
        let version = found.as_ref().map(ObjectVersion::from).filter(|version| {
            let oversized = self.spec.max_size.is_some_and(|max| version.size > max);
            if oversized {
                tracing::warn!(
                    "[{label}] skipping oversized object `{}` ({} bytes)",
                    probe.key,
                    version.size
                );
            }
            !oversized
        });
        let old = slot.files.remove(&probe.lead);
        let listed = match &old {
            Some(FileState::Settled { version, .. }) if slot.published => Some(version.clone()),
            _ => None,
        };
        let settles_at_once = |version: &ObjectVersion| match self.spec.settling {
            Settling::OnSight => true,
            Settling::Stable { min_age } => version
                .last_modified
                .is_some_and(|modified| now - modified >= min_age),
        };
        let new = match (old, version) {
            (_, None) => FileState::Absent,
            (Some(FileState::Settled { version: old, .. }), Some(version)) if old == version => {
                FileState::Settled {
                    version,
                    recheck: false,
                }
            }
            (Some(FileState::Unsettled(mut pending)), Some(version)) => {
                if pending.observe(version.clone()) || settles_at_once(&version) {
                    FileState::Settled {
                        version,
                        recheck: false,
                    }
                } else {
                    FileState::Unsettled(pending)
                }
            }
            (_, Some(version)) if settles_at_once(&version) => FileState::Settled {
                version,
                recheck: false,
            },
            (_, Some(version)) => FileState::Unsettled(PendingFile::new(version)),
        };
        if sentinel && !matches!(new, FileState::Absent) && slot.sentinel_seen.is_none() {
            slot.sentinel_seen = Some(now);
        }
        let listing = match &new {
            FileState::Settled { version, .. } if slot.published => Some(version.clone()),
            _ => None,
        };
        if listed != listing {
            let file = |version| ProbedFile {
                time: probe.time,
                lead: probe.lead,
                key: probe.key.clone(),
                version,
            };
            if let Some(version) = listed {
                if sentinel && !self.spec.immutable {
                    // The sentinel was replaced or removed: so may the
                    // others be.
                    for state in slot.files.values_mut() {
                        if let FileState::Settled { recheck, .. } = state {
                            *recheck = true;
                        }
                    }
                }
                report.withdrawn.push(file(version));
            }
            if let Some(version) = listing {
                report.added.push(file(version));
            }
        }
        slot.files.insert(probe.lead, new);
    }

    /// Publish every unpublished slot that is complete or whose grace has
    /// passed.
    fn publish(&mut self, now: DateTime<Utc>, report: &mut ProbeReport) {
        let mut published = Vec::new();
        for (&time, slot) in self.slots.iter_mut().rev() {
            if slot.published {
                continue;
            }
            let (settled, missing): (Vec<Option<u32>>, Vec<Option<u32>>) = self
                .leads
                .iter()
                .partition(|lead| matches!(slot.files.get(lead), Some(FileState::Settled { .. })));
            if settled.is_empty() {
                continue;
            }
            // Partial only once every missing lead has been answered, so a
            // HEAD cap still working through a run does not cut it short.
            let graced = slot
                .sentinel_seen
                .is_some_and(|seen| now - seen >= self.spec.run_grace)
                && missing.iter().all(|lead| slot.files.contains_key(lead));
            if !missing.is_empty() && !graced {
                continue;
            }
            if !missing.is_empty() {
                let missing: Vec<String> = missing
                    .iter()
                    .map(|lead| lead.map_or_else(String::new, |lead| lead.to_string()))
                    .collect();
                tracing::warn!(
                    "[{}] publishing {} with {} of {} leads: leads {} have not settled {} min \
                     after its first file appeared",
                    self.spec.label,
                    time.to_rfc3339(),
                    settled.len(),
                    self.leads.len(),
                    missing.join(", "),
                    self.spec.run_grace.num_minutes()
                );
            }
            slot.published = true;
            slot.checked = Some(now);
            published.push(time);
        }
        for time in published {
            report.published.push(time);
            if let Some(slot) = self.slots.get(&time) {
                report.added.extend(self.settled(time, slot));
            }
        }
    }

    /// The settled files of `slot`, in lead order.
    fn settled(&self, time: DateTime<Utc>, slot: &Slot) -> Vec<ProbedFile> {
        slot.files
            .iter()
            .filter_map(|(&lead, state)| match state {
                FileState::Settled { version, .. } => Some(ProbedFile {
                    time,
                    lead,
                    key: self.key(time, lead),
                    version: version.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

impl Slot {
    /// Whether `lead`'s file has been seen and not lost since.
    fn is_seen(&self, lead: Option<u32>) -> bool {
        matches!(
            self.files.get(&lead),
            Some(FileState::Unsettled(_) | FileState::Settled { .. })
        )
    }
}

/// Check at construction that `spec` renders one key per file of each slot.
fn validate(spec: &TemplateSpec) -> Result<(), DataServerError> {
    let invalid = |why: String| {
        DataServerError::Config(format!("[{}] template discovery: {why}", spec.label))
    };
    let (prefix, filename) = (&spec.prefix_pattern, &spec.filename_template);
    let bare = filename
        .replace(RUN_PLACEHOLDER, "")
        .replace(LEAD_PLACEHOLDER, "");
    if bare.contains(['{', '}']) {
        return Err(invalid(format!(
            "filename_template '{filename}': '{{' and '}}' are only allowed as \
             {RUN_PLACEHOLDER} and {LEAD_PLACEHOLDER}"
        )));
    }
    // Whether the keys name the slot time at all.
    let dated = match &spec.schedule {
        Schedule::Cadence(cadence) => {
            if *cadence <= Duration::zero() {
                return Err(invalid("the cadence must be positive".into()));
            }
            if prefix.contains(['{', '}']) || filename.contains(RUN_PLACEHOLDER) {
                return Err(invalid(format!(
                    "{RUN_PLACEHOLDER} needs a run schedule, and the prefix_pattern takes \
                     no placeholders"
                )));
            }
            let mut dated = prefix_step(prefix)? != PrefixStep::Fixed;
            for item in StrftimeItems::new(&bare) {
                match item {
                    Item::Error => {
                        return Err(invalid(format!(
                            "filename_template '{filename}' has an unknown strftime specifier"
                        )))
                    }
                    Item::Numeric(..) | Item::Fixed(..) => dated = true,
                    _ => {}
                }
            }
            dated
        }
        Schedule::Runs { run_hours } => {
            if spec.time_window.is_none() {
                return Err(invalid("a run schedule needs a time_window".into()));
            }
            if run_hours.is_empty() || run_hours.iter().any(|&hour| hour > 23) {
                return Err(invalid(format!(
                    "run hours {run_hours:?}: at least one, each 0-23"
                )));
            }
            if !prefix.contains(RUN_PLACEHOLDER) && !filename.contains(RUN_PLACEHOLDER) {
                return Err(invalid(format!(
                    "a run's keys name its hour with {RUN_PLACEHOLDER}"
                )));
            }
            let prefix_dates = validate_run_prefix_pattern(prefix, run_hours)?;
            let only_dates = || {
                invalid(format!(
                    "filename_template '{filename}' may use only strftime date specifiers; \
                     the run hour is {RUN_PLACEHOLDER}"
                ))
            };
            let filename_step = prefix_step(&bare).map_err(|_| only_dates())?;
            if filename_step == PrefixStep::Hour {
                return Err(only_dates());
            }
            prefix_dates == PrefixStep::Day || filename_step == PrefixStep::Day
        }
    };
    if !dated {
        return Err(invalid(
            "the keys name no time, so every slot would probe the same file".into(),
        ));
    }
    let lead_count = filename.matches(LEAD_PLACEHOLDER).count();
    if lead_count > 1 || (lead_count == 1) == spec.leads.is_empty() {
        return Err(invalid(format!(
            "the filename_template names {LEAD_PLACEHOLDER} once exactly when leads are \
             configured"
        )));
    }
    let limit = 10u32.pow(LEAD_DIGITS as u32);
    if let Some(lead) = spec.leads.iter().find(|&&lead| lead >= limit) {
        return Err(invalid(format!(
            "lead {lead} does not fit {LEAD_DIGITS} digits"
        )));
    }
    if spec.leads.iter().collect::<BTreeSet<_>>().len() != spec.leads.len() {
        return Err(invalid("a lead is listed twice".into()));
    }
    if spec.max_heads_per_poll == 0 {
        return Err(invalid("max_heads_per_poll must be at least 1".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::FilenameMatcher;
    use crate::test_store::MetaStore;
    use std::sync::Arc;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// MEPS per-lead files on thredds.met.no's fileServer (#1067).
    const BASE: &str = "thredds/fileServer";
    const PREFIX: &str = "meps25epsarchive/%Y/%m/%d/{run}/member_00";
    const FILENAME: &str = "meps_sfc_{lead}_%Y%m%dT{run}Z.nc";
    const RUN_06: &str = "2026-10-17T06:00:00Z";
    const RUN_03: &str = "2026-10-17T03:00:00Z";

    fn meps_spec(leads: u32) -> TemplateSpec {
        TemplateSpec {
            prefix_pattern: PREFIX.into(),
            leads: (0..leads).collect(),
            time_window: Some(TimeWindow::parse("-PT6H").unwrap()),
            ..TemplateSpec::new(
                BASE,
                FILENAME,
                Schedule::Runs {
                    run_hours: (0..24).step_by(3).collect(),
                },
                "meps",
            )
        }
    }

    fn meps_key(run: &str, lead: u32) -> String {
        let run = at(run);
        format!(
            "{BASE}/meps25epsarchive/{}/member_00/meps_sfc_{lead:02}_{}Z.nc",
            run.format("%Y/%m/%d/%H"),
            run.format("%Y%m%dT%H")
        )
    }

    /// A store and a prober over it.
    fn setup(spec: TemplateSpec) -> (Arc<MetaStore>, DataStore, TemplateProber) {
        let meta = Arc::new(MetaStore::default());
        let store = DataStore::new(meta.clone());
        (meta, store, TemplateProber::new(spec).unwrap())
    }

    /// Put `leads` of `run` in the store, `size` bytes, modified at `modified`.
    fn upload(
        meta: &MetaStore,
        run: &str,
        leads: impl IntoIterator<Item = u32>,
        size: u64,
        modified: &str,
    ) {
        for lead in leads {
            meta.set(&meps_key(run, lead), size, Some(at(modified)));
        }
    }

    fn keys(files: &[ProbedFile]) -> Vec<String> {
        files.iter().map(|file| file.key.clone()).collect()
    }

    /// The real layout renders the real keys, and a listable mirror of it
    /// is read back by the matcher: lead and run time.
    #[test]
    fn meps_layout_renders_and_matches() {
        let prober = TemplateProber::new(meps_spec(67)).unwrap();
        let run = at(RUN_06);
        assert_eq!(
            prober.key(run, Some(0)),
            "thredds/fileServer/meps25epsarchive/2026/10/17/06/member_00/\
             meps_sfc_00_20261017T06Z.nc"
        );
        assert_eq!(
            prober.key(at("2026-10-17T21:00:00Z"), Some(66)),
            "thredds/fileServer/meps25epsarchive/2026/10/17/21/member_00/\
             meps_sfc_66_20261017T21Z.nc"
        );
        let matcher = FilenameMatcher::from_template(FILENAME).unwrap();
        assert_eq!(
            matcher.match_file("meps_sfc_66_20261017T06Z.nc"),
            Some(Ok((run, Some(66))))
        );
        // The runs in a 6 h window at 08:10, newest first: 00 UTC is before it.
        assert_eq!(
            prober.slot_times(at("2026-10-17T08:10:00Z")),
            [at(RUN_06), at(RUN_03)]
        );
    }

    /// An unknown run costs one HEAD, its sentinel's, until the sentinel
    /// exists; then its other leads are HEADed and the run is published
    /// whole. A published, immutable source then probes only the run that
    /// has not appeared.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_run_probes_its_sentinel_only() {
        let (meta, store, mut prober) = setup(TemplateSpec {
            immutable: true,
            ..meps_spec(67)
        });
        // Run 06 is complete and old enough to settle on sight; run 03 is
        // missing.
        upload(&meta, RUN_06, 0..67, 100, "2026-10-17T07:55:00Z");
        let now = at("2026-10-17T08:10:00Z");

        let report = prober.poll(&store, now).unwrap();
        assert_eq!(
            meta.take_headed(),
            [meps_key(RUN_03, 0), meps_key(RUN_06, 0)],
            "one sentinel per unknown run"
        );
        assert!(prober.files().is_empty(), "a run is published whole");
        assert!(!report.changed());

        let report = prober.poll(&store, now).unwrap();
        assert_eq!(
            report.heads, 67,
            "run 06's 66 other leads and run 03's sentinel"
        );
        assert_eq!(report.published, [at(RUN_06)]);
        assert_eq!(report.added.len(), 67);
        assert_eq!(
            keys(&prober.files()),
            (0..67).map(|l| meps_key(RUN_06, l)).collect::<Vec<_>>()
        );
        meta.take_headed();

        let report = prober.poll(&store, now).unwrap();
        assert_eq!(meta.take_headed(), [meps_key(RUN_03, 0)]);
        assert!(!report.changed());
    }

    /// A fresh file settles once two more polls see the same size and
    /// `Last-Modified`; a change restarts it. One older than `min_age`
    /// settles on sight; one without `Last-Modified` only by polls.
    #[tokio::test(flavor = "multi_thread")]
    async fn files_settle_on_unchanged_polls_or_age() {
        let (meta, store, mut prober) = setup(meps_spec(2));
        let t0 = at("2026-10-17T08:00:00Z");
        let minute = |n| t0 + Duration::minutes(n);
        upload(&meta, RUN_06, [0, 1], 100, "2026-10-17T08:00:00Z");

        prober.poll(&store, minute(0)).unwrap(); // sentinel seen
        prober.poll(&store, minute(1)).unwrap(); // sentinel 1, lead 1 seen
                                                 // Lead 1 is still being written.
        upload(&meta, RUN_06, [1], 150, "2026-10-17T08:01:30Z");
        prober.poll(&store, minute(2)).unwrap(); // sentinel settled, lead 1 restarts
        prober.poll(&store, minute(3)).unwrap();
        assert!(prober.files().is_empty(), "lead 1 has not settled");
        let report = prober.poll(&store, minute(4)).unwrap();
        assert_eq!(report.published, [at(RUN_06)]);
        assert_eq!(prober.files()[1].version.size, 150);

        // `min_age` (10 min) old on first sight: settled at once.
        let (meta, store, mut prober) = setup(meps_spec(1));
        upload(&meta, RUN_06, [0], 100, "2026-10-17T07:50:00Z");
        let report = prober.poll(&store, minute(0)).unwrap();
        assert_eq!(report.published, [at(RUN_06)]);

        // No `Last-Modified`: never old, so settled by polls only.
        let (meta, store, mut prober) = setup(meps_spec(1));
        meta.set(&meps_key(RUN_06, 0), 100, None);
        for n in 0..2 {
            prober.poll(&store, minute(n)).unwrap();
            assert!(prober.files().is_empty());
        }
        prober.poll(&store, minute(2)).unwrap();
        assert_eq!(prober.files().len(), 1);
    }

    /// The per-poll cap counts HEADs. The newest run's leads go first, the
    /// rest wait, and no run is published partially while some of its leads
    /// have not been HEADed, even with no grace period at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_cap_counts_heads_newest_run_first() {
        let (meta, store, mut prober) = setup(TemplateSpec {
            max_heads_per_poll: 20,
            run_grace: Duration::zero(),
            ..meps_spec(67)
        });
        upload(&meta, RUN_06, 0..67, 100, "2026-10-17T07:00:00Z");
        upload(&meta, RUN_03, 0..67, 100, "2026-10-17T04:00:00Z");
        let now = at("2026-10-17T08:10:00Z");

        assert_eq!(prober.poll(&store, now).unwrap().heads, 2);
        meta.take_headed();
        let report = prober.poll(&store, now).unwrap();
        assert_eq!((report.heads, report.deferred), (20, 112));
        let mut expected: Vec<String> = (1..21).map(|l| meps_key(RUN_06, l)).collect();
        expected.sort();
        assert_eq!(meta.take_headed(), expected);
        assert!(
            prober.files().is_empty(),
            "47 leads of run 06 not HEADed yet"
        );

        let mut polls = 2;
        while prober.files().len() < 134 {
            let report = prober.poll(&store, now).unwrap();
            assert!(report.heads <= 20);
            polls += 1;
            assert!(polls < 20, "the cap must still make progress");
        }
        // The 2 sentinels, then 132 leads at 20 per poll.
        assert_eq!(polls, 1 + 7);
    }

    /// A run with 39 of its 67 leads is published `run_grace` after its
    /// sentinel appeared, with those 39, and the 28 missing leads are then
    /// re-HEADed only with the slow recheck, which finds a late one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_partial_run_is_published_after_its_grace() {
        let (meta, store, mut prober) = setup(TemplateSpec {
            time_window: Some(TimeWindow::parse("-PT4H").unwrap()),
            ..meps_spec(67)
        });
        let t0 = at("2026-10-17T08:00:00Z");
        let minute = |n| t0 + Duration::minutes(n);
        upload(&meta, RUN_06, 0..39, 100, "2026-10-17T08:00:00Z");

        for n in 0..30 {
            let report = prober.poll(&store, minute(n)).unwrap();
            assert!(report.published.is_empty(), "minute {n}");
        }
        assert!(prober.files().is_empty());
        let report = prober.poll(&store, minute(30)).unwrap();
        assert_eq!(report.published, [at(RUN_06)]);
        assert_eq!(report.added.len(), 39);
        assert_eq!(
            keys(&prober.files()),
            (0..39).map(|l| meps_key(RUN_06, l)).collect::<Vec<_>>()
        );

        // Published: nothing is probed until the recheck is due.
        assert_eq!(prober.poll(&store, minute(31)).unwrap().heads, 0);
        upload(&meta, RUN_06, [50], 100, "2026-10-17T08:59:00Z");
        meta.take_headed();
        let report = prober.poll(&store, minute(60)).unwrap();
        assert_eq!(
            report.heads,
            1 + 28 + 1,
            "the sentinel, the missing leads and the new 09 UTC run's sentinel"
        );
        assert!(meta.take_headed().contains(&meps_key(RUN_06, 0)));
        prober.poll(&store, minute(61)).unwrap();
        let report = prober.poll(&store, minute(62)).unwrap();
        assert_eq!(keys(&report.added), [meps_key(RUN_06, 50)]);
        assert_eq!(prober.files().len(), 40);
    }

    /// A settled file replaced in place: the slow recheck sees the
    /// sentinel's new size, withdraws it and re-HEADs the run's other files,
    /// which withdraws a lead whose `Last-Modified` moved. Both come back as
    /// their new versions once settled. An immutable source never looks.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_replaced_file_is_withdrawn_and_settles_again() {
        let spec = TemplateSpec {
            time_window: Some(TimeWindow::parse("-PT3H").unwrap()),
            ..meps_spec(3)
        };
        let (meta, store, mut prober) = setup(spec.clone());
        let t0 = at("2026-10-17T08:00:00Z");
        let minute = |n| t0 + Duration::minutes(n);
        upload(&meta, RUN_06, 0..3, 100, "2026-10-17T07:00:00Z");
        prober.poll(&store, minute(0)).unwrap();
        prober.poll(&store, minute(1)).unwrap();
        assert_eq!(prober.files().len(), 3);

        upload(&meta, RUN_06, [0], 120, "2026-10-17T08:30:00Z");
        upload(&meta, RUN_06, [1], 100, "2026-10-17T08:30:00Z");
        assert_eq!(prober.poll(&store, minute(20)).unwrap().heads, 0, "not due");
        let report = prober.poll(&store, minute(31)).unwrap();
        assert_eq!(keys(&report.withdrawn), [meps_key(RUN_06, 0)]);
        assert_eq!(report.withdrawn[0].version.size, 100);
        let report = prober.poll(&store, minute(32)).unwrap();
        assert_eq!(
            report.heads, 3,
            "the replaced sentinel, and both other leads to recheck"
        );
        assert_eq!(keys(&report.withdrawn), [meps_key(RUN_06, 1)]);
        assert_eq!(prober.files().len(), 1, "lead 2 is unchanged");
        for n in 33..36 {
            prober.poll(&store, minute(n)).unwrap();
        }
        let files = prober.files();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].version.size, 120);
        assert_eq!(
            files[1].version.last_modified,
            Some(at("2026-10-17T08:30:00Z"))
        );

        // Immutable: the same replacement is never seen.
        let (meta, store, mut prober) = setup(TemplateSpec {
            immutable: true,
            ..spec
        });
        upload(&meta, RUN_06, 0..3, 100, "2026-10-17T07:00:00Z");
        prober.poll(&store, minute(0)).unwrap();
        prober.poll(&store, minute(1)).unwrap();
        upload(&meta, RUN_06, [0], 120, "2026-10-17T08:30:00Z");
        for n in [31, 45, 59] {
            assert_eq!(prober.poll(&store, minute(n)).unwrap().heads, 0);
        }
        assert_eq!(prober.files()[0].version.size, 100);
    }

    /// A run that leaves the window is withdrawn; a failed HEAD leaves its
    /// file as it was.
    #[tokio::test(flavor = "multi_thread")]
    async fn aged_out_runs_are_withdrawn_and_failures_keep_state() {
        let (meta, store, mut prober) = setup(TemplateSpec {
            time_window: Some(TimeWindow::parse("-PT3H").unwrap()),
            ..meps_spec(2)
        });
        upload(&meta, RUN_06, 0..2, 100, "2026-10-17T07:00:00Z");
        meta.failing.lock().unwrap().insert(meps_key(RUN_06, 1));
        let now = at("2026-10-17T08:00:00Z");
        prober.poll(&store, now).unwrap();
        let report = prober.poll(&store, now).unwrap();
        assert_eq!(report.failed, 1);
        assert!(prober.files().is_empty());
        meta.failing.lock().unwrap().clear();
        prober.poll(&store, now).unwrap();
        assert_eq!(prober.files().len(), 2);

        let report = prober.poll(&store, at("2026-10-17T09:01:00Z")).unwrap();
        assert_eq!(report.withdrawn.len(), 2);
        assert!(prober.files().is_empty());
    }

    /// A published run's sentinel that vanished is withdrawn and has the
    /// run's other files re-HEADed, which withdraws the one that went too.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_vanished_sentinel_has_its_run_rechecked() {
        let (meta, store, mut prober) = setup(TemplateSpec {
            time_window: Some(TimeWindow::parse("-PT4H").unwrap()),
            ..meps_spec(3)
        });
        let t0 = at("2026-10-17T08:00:00Z");
        upload(&meta, RUN_06, 0..3, 100, "2026-10-17T07:00:00Z");
        prober.poll(&store, t0).unwrap();
        prober.poll(&store, t0).unwrap();
        assert_eq!(prober.files().len(), 3);

        meta.remove(&meps_key(RUN_06, 0));
        meta.remove(&meps_key(RUN_06, 2));
        let report = prober.poll(&store, t0 + SETTLED_RECHECK).unwrap();
        assert_eq!(keys(&report.withdrawn), [meps_key(RUN_06, 0)]);
        let report = prober.poll(&store, t0 + SETTLED_RECHECK).unwrap();
        assert_eq!(report.heads, 2, "leads 1 and 2, rechecked");
        assert_eq!(keys(&report.withdrawn), [meps_key(RUN_06, 2)]);
        assert_eq!(keys(&prober.files()), [meps_key(RUN_06, 1)]);
    }

    /// Keys that cannot name one file per slot and lead are config errors.
    #[test]
    fn specs_that_cannot_be_probed_are_rejected() {
        let runs = || meps_spec(67);
        let rejected = |spec: TemplateSpec| TemplateProber::new(spec).is_err();
        assert!(!rejected(runs()));
        assert!(rejected(TemplateSpec {
            time_window: None,
            ..runs()
        }));
        assert!(
            rejected(TemplateSpec {
                filename_template: "meps_sfc_{lead}_%Y%m%dT%HZ.nc".into(),
                ..runs()
            }),
            "the run hour is {{run}}"
        );
        assert!(rejected(TemplateSpec {
            filename_template: "meps_sfc_{Lead}_%Y%m%dT{run}Z.nc".into(),
            ..runs()
        }));
        assert!(rejected(TemplateSpec {
            leads: vec![],
            ..runs()
        }));
        assert!(rejected(TemplateSpec {
            leads: vec![0, 100],
            ..runs()
        }));
        assert!(rejected(TemplateSpec {
            leads: vec![0, 1, 0],
            ..runs()
        }));
        assert!(rejected(TemplateSpec {
            schedule: Schedule::Runs {
                run_hours: vec![24]
            },
            ..runs()
        }));
        assert!(rejected(TemplateSpec {
            max_heads_per_poll: 0,
            ..runs()
        }));
        let cadence = |template: &str| {
            TemplateSpec::new("", template, Schedule::Cadence(Duration::minutes(5)), "t")
        };
        assert!(!rejected(cadence("composite_hx_%Y%m%d_%H%M-hd5")));
        assert!(rejected(cadence("composite_hx_latest-hd5")));
        assert!(rejected(cadence("composite_{run}_%Y%m%d-hd5")));
        assert!(rejected(cadence("composite_%Y%m%d_%!-hd5")));
        assert!(rejected(TemplateSpec {
            schedule: Schedule::Cadence(Duration::zero()),
            ..cadence("c_%Y%m%d%H%M.h5")
        }));
    }
}
