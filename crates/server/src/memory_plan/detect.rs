//! The memory the process may use (#1312), from the first of:
//!
//! 1. `[memory] total_mb`, with a WARN when it is above a cgroup limit;
//! 2. the cgroup limit × 0.8: the smallest finite `memory.max` or
//!    `memory.high` of the process's cgroup v2 and its ancestors, else the
//!    v1 `memory.limit_in_bytes` or `hierarchical_memory_limit`. The cgroup
//!    also counts the page cache of mapped local data, and crossing the
//!    limit is an OOM kill: hence the 20 % kept back;
//! 3. without a limit, `min(MemTotal × 0.5, MemAvailable at boot × 0.8)`,
//!    with a WARN naming it and `total_mb`.
//!
//! Under [`MIN_TOTAL`] the plan puts everything at its floor, with a WARN.
//!
//! Linux reads `/proc` and `/sys` under a root ([`read`]), so the tests run
//! over fixture trees on any OS. Where the root has no `/proc/meminfo`,
//! macOS and Windows ask the system for the host's memory.

use std::fmt;
use std::path::{Path, PathBuf};

use ds_cache::plan::grant::{GIB, MIN_TOTAL};
use ds_cache::MIB;

/// The share of a cgroup limit the plan takes, in percent.
const CGROUP_PERCENT: u64 = 80;
/// The share of the host's memory the plan takes without a limit.
const MEM_TOTAL_PERCENT: u64 = 50;
/// The share of the memory available at boot it takes at most.
const MEM_AVAILABLE_PERCENT: u64 = 80;

/// A cgroup's memory limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Limit {
    /// 1 or 2.
    pub version: u8,
    /// The file that set it: `memory.max`, `memory.high`,
    /// `memory.limit_in_bytes` or `hierarchical_memory_limit`.
    pub file: &'static str,
    pub bytes: u64,
}

/// What the system says about memory, read once at boot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reading {
    /// The smallest finite limit of the process's cgroup.
    pub limit: Option<Limit>,
    /// The cgroup's protection (`memory.low`, v1
    /// `memory.soft_limit_in_bytes`): what Docker's `mem_reservation` and
    /// Kubernetes requests set. Reported, not used: it is not a limit.
    pub low_bytes: Option<u64>,
    /// `MemTotal`.
    pub host_bytes: Option<u64>,
    /// `MemAvailable`, at boot.
    pub available_bytes: Option<u64>,
}

/// Where the total came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    Configured,
    Cgroup(Limit),
    Host {
        host_bytes: u64,
        available_bytes: Option<u64>,
    },
}

/// The memory the process may use, and where that came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Total {
    pub bytes: u64,
    pub source: Source,
    /// WARNs to log once.
    pub warnings: Vec<String>,
}

/// `bytes` in GiB, to two decimals.
pub(crate) fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / GIB as f64)
}

impl fmt::Display for Source {
    /// The input that set the total, as the plan log names it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Configured => f.write_str("[memory] total_mb"),
            Source::Cgroup(limit) => write!(
                f,
                "cgroup v{} {} {} × 0.8",
                limit.version,
                limit.file,
                gib(limit.bytes)
            ),
            Source::Host {
                host_bytes,
                available_bytes: Some(available),
            } => write!(
                f,
                "min(MemTotal {} × 0.5, MemAvailable at boot {} × 0.8)",
                gib(*host_bytes),
                gib(*available)
            ),
            Source::Host {
                host_bytes,
                available_bytes: None,
            } => write!(f, "MemTotal {} × 0.5", gib(*host_bytes)),
        }
    }
}

/// `bytes` × `percent` / 100, in whole MiB down.
fn share(bytes: u64, percent: u64) -> u64 {
    (bytes / 100 * percent + bytes % 100 * percent / 100) / MIB * MIB
}

/// The total from `configured_mb` (`[memory] total_mb`) and `reading`;
/// `None` when neither is known.
pub(crate) fn total(configured_mb: Option<u64>, reading: &Reading) -> Option<Total> {
    let mut warnings = Vec::new();
    let (bytes, source) = if let Some(mb) = configured_mb {
        let bytes = mb.saturating_mul(MIB);
        if let Some(limit) = reading.limit.as_ref().filter(|l| bytes > l.bytes) {
            warnings.push(format!(
                "[memory] total_mb = {mb} is above the cgroup v{} {} of {}: the process is \
                 killed when it crosses the limit",
                limit.version,
                limit.file,
                gib(limit.bytes)
            ));
        }
        (bytes, Source::Configured)
    } else if let Some(limit) = &reading.limit {
        (
            share(limit.bytes, CGROUP_PERCENT),
            Source::Cgroup(limit.clone()),
        )
    } else {
        let host_bytes = reading.host_bytes?;
        let of_host = share(host_bytes, MEM_TOTAL_PERCENT);
        let bytes = reading
            .available_bytes
            .map_or(of_host, |a| of_host.min(share(a, MEM_AVAILABLE_PERCENT)));
        let source = Source::Host {
            host_bytes,
            available_bytes: reading.available_bytes,
        };
        warnings.push(format!(
            "No memory limit: the memory plan takes {} ({source}). Set [memory] total_mb \
             to the memory this server may use",
            gib(bytes)
        ));
        (bytes, source)
    };
    if bytes < MIN_TOTAL {
        warnings.push(format!(
            "A memory total of {} MiB ({source}) is under 512 MiB: the plan puts every cache \
             and budget at its floor",
            bytes / MIB
        ));
    }
    Some(Total {
        bytes,
        source,
        warnings,
    })
}

/// Read the memory facts under `root` (`/` in the server).
pub(crate) fn read(root: &Path) -> Reading {
    let meminfo = std::fs::read_to_string(root.join("proc/meminfo")).ok();
    let (host_bytes, available_bytes) = match &meminfo {
        Some(meminfo) => (
            meminfo_kib(meminfo, "MemTotal:"),
            meminfo_kib(meminfo, "MemAvailable:"),
        ),
        None => platform::host_memory(),
    };
    let (limit, low_bytes) = cgroup(root, host_bytes);
    Reading {
        limit,
        low_bytes,
        host_bytes,
        available_bytes,
    }
}

/// A `/proc/meminfo` field, in bytes.
fn meminfo_kib(meminfo: &str, key: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with(key))?;
    let kib: u64 = line[key.len()..].split_whitespace().next()?.parse().ok()?;
    Some(kib.saturating_mul(1024))
}

/// A mounted cgroup hierarchy: its mount point and the cgroup path that is
/// its root (a container's own cgroup, without a cgroup namespace).
struct Mount {
    point: PathBuf,
    root: String,
}

/// The mount of the cgroup v2 hierarchy (`v1_memory` false), or of the v1
/// memory controller, from `/proc/self/mountinfo`.
fn mount(mountinfo: &str, v1_memory: bool) -> Option<Mount> {
    mountinfo.lines().find_map(|line| {
        let (fields, rest) = line.split_once(" - ")?;
        let fields: Vec<&str> = fields.split_whitespace().collect();
        let mut rest = rest.split_whitespace();
        let fstype = rest.next()?;
        let options = rest.nth(1).unwrap_or("");
        let wanted = if v1_memory {
            fstype == "cgroup" && options.split(',').any(|o| o == "memory")
        } else {
            fstype == "cgroup2"
        };
        if !wanted {
            return None;
        }
        Some(Mount {
            root: fields.get(3)?.to_string(),
            point: PathBuf::from(fields.get(4)?),
        })
    })
}

/// The directory of cgroup `path` in `mount` under `root`; the mount point
/// itself when `path` lies outside the mount's root.
fn cgroup_dir(root: &Path, mount: &Mount, path: &str) -> PathBuf {
    let point = root.join(mount.point.strip_prefix("/").unwrap_or(&mount.point));
    let relative = path
        .strip_prefix(mount.root.trim_end_matches('/'))
        .unwrap_or("")
        .trim_start_matches('/');
    let dir = point.join(relative);
    if dir.is_dir() {
        dir
    } else {
        point
    }
}

/// A limit file's value: `None` for `max`, unreadable, or at least
/// `host_bytes` (v1's "unlimited" is 9 223 372 036 854 771 712).
fn limit_value(text: &str, host_bytes: Option<u64>) -> Option<u64> {
    let bytes: u64 = text.trim().parse().ok()?;
    (host_bytes.is_none_or(|host| bytes < host) && bytes > 0).then_some(bytes)
}

/// The smallest of two limits.
fn smaller(a: Option<Limit>, b: Option<Limit>) -> Option<Limit> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.bytes < a.bytes { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// The process's cgroup limit and protection: v2 first, then v1.
fn cgroup(root: &Path, host_bytes: Option<u64>) -> (Option<Limit>, Option<u64>) {
    let Ok(cgroups) = std::fs::read_to_string(root.join("proc/self/cgroup")) else {
        return (None, None);
    };
    let mountinfo = std::fs::read_to_string(root.join("proc/self/mountinfo")).unwrap_or_default();
    let read = |dir: &Path, file: &str| std::fs::read_to_string(dir.join(file)).ok();
    let mut low = None;
    if let Some(path) = cgroups.lines().find_map(|l| l.strip_prefix("0::")) {
        let mount = mount(&mountinfo, false).unwrap_or(Mount {
            point: PathBuf::from("/sys/fs/cgroup"),
            root: "/".into(),
        });
        let top = cgroup_dir(root, &mount, "");
        let leaf = cgroup_dir(root, &mount, path);
        low = read(&leaf, "memory.low").and_then(|t| limit_value(&t, host_bytes));
        let mut limit = None;
        let mut dir = Some(leaf.as_path());
        while let Some(at) = dir {
            for file in ["memory.max", "memory.high"] {
                let bytes = read(at, file).and_then(|t| limit_value(&t, host_bytes));
                let found = bytes.map(|bytes| Limit {
                    version: 2,
                    file,
                    bytes,
                });
                limit = smaller(limit, found);
            }
            dir = (at != top).then(|| at.parent()).flatten();
        }
        if limit.is_some() {
            return (limit, low);
        }
    }
    let v1 = cgroups.lines().find_map(|line| {
        let mut parts = line.splitn(3, ':');
        let controllers = parts.nth(1)?;
        controllers
            .split(',')
            .any(|c| c == "memory")
            .then(|| parts.next())
            .flatten()
    });
    if let Some(path) = v1 {
        let mount = mount(&mountinfo, true).unwrap_or(Mount {
            point: PathBuf::from("/sys/fs/cgroup/memory"),
            root: "/".into(),
        });
        let dir = cgroup_dir(root, &mount, path);
        let limit = read(&dir, "memory.limit_in_bytes")
            .and_then(|t| limit_value(&t, host_bytes))
            .map(|bytes| Limit {
                version: 1,
                file: "memory.limit_in_bytes",
                bytes,
            });
        let hierarchical = read(&dir, "memory.stat")
            .and_then(|stat| {
                stat.lines()
                    .find_map(|l| l.strip_prefix("hierarchical_memory_limit "))
                    .and_then(|t| limit_value(t, host_bytes))
            })
            .map(|bytes| Limit {
                version: 1,
                file: "hierarchical_memory_limit",
                bytes,
            });
        let soft =
            read(&dir, "memory.soft_limit_in_bytes").and_then(|t| limit_value(&t, host_bytes));
        return (smaller(limit, hierarchical), low.or(soft));
    }
    (None, low)
}

/// The host's memory from the system, where there is no `/proc/meminfo`:
/// (total, available).
mod platform {
    #[cfg(target_os = "macos")]
    pub(super) fn host_memory() -> (Option<u64>, Option<u64>) {
        /// A sysctl of `size` bytes by name.
        fn sysctl<T: Default + Copy>(name: &std::ffi::CStr) -> Option<T> {
            let mut value = T::default();
            let mut len = std::mem::size_of::<T>();
            // SAFETY: `value` is a `T` and `len` its size; the call writes
            // at most `len` bytes and stores the length written.
            let rc = unsafe {
                libc::sysctlbyname(
                    name.as_ptr(),
                    (&mut value as *mut T).cast(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            (rc == 0 && len == std::mem::size_of::<T>()).then_some(value)
        }
        let total: Option<u64> = sysctl(c"hw.memsize");
        // The share of memory the kernel considers free, in percent: what
        // memory pressure reads.
        let level: Option<u32> = sysctl(c"kern.memorystatus_level");
        let available = total
            .zip(level.filter(|l| *l <= 100))
            .map(|(total, level)| total / 100 * u64::from(level));
        (total, available)
    }

    #[cfg(windows)]
    pub(super) fn host_memory() -> (Option<u64>, Option<u64>) {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..MEMORYSTATUSEX::default()
        };
        // SAFETY: `status` is a MEMORYSTATUSEX with its length set, as the
        // call requires.
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return (None, None);
        }
        (Some(status.ullTotalPhys), Some(status.ullAvailPhys))
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub(super) fn host_memory() -> (Option<u64>, Option<u64>) {
        (None, None)
    }
}
