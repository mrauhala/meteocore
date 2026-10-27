//! jemalloc's runtime options, set once at startup (#1310).
//!
//! Without background threads an arena purges its dirty pages only when it
//! is next used, so a quiet arena keeps freed memory resident: production
//! held 0.9 GiB of resident pages above the active ones. [`configure`] turns
//! them on with one `mallctl` write before any Tokio runtime starts, keeps
//! jemalloc's decay defaults, and logs the effective options. jemalloc runs
//! at most four background threads by default (`max_background_threads`).
//!
//! The write happens at runtime, not through a build-time `malloc_conf`, so
//! it cannot silently depend on a build environment variable, and so a
//! platform without background threads (macOS) gets an INFO line instead of
//! a jemalloc that refuses to initialise.

use tikv_jemalloc_ctl::{background_thread, opt, raw};
use tracing::{info, warn};

/// The environment variable jemalloc reads its options from. Its symbols
/// are prefixed `_rjem_`, so plain `MALLOC_CONF` is never read.
const CONF_ENV: &str = "_RJEM_MALLOC_CONF";

/// Turns on background purging, unless `_RJEM_MALLOC_CONF` sets
/// `background_thread` itself, and logs the effective options. Returns
/// whether background threads are on. Never panics: where jemalloc has no
/// background threads the write fails with `ENOENT`, logged at INFO; any
/// other failure is unexpected and logged at WARN.
pub(crate) fn configure() -> bool {
    let conf = std::env::var(CONF_ENV).ok();
    if !conf.as_deref().is_some_and(sets_background_thread) {
        match background_thread::write(true) {
            Ok(()) => {}
            Err(e) if is_enoent(e) => {
                info!("jemalloc background purging is not supported on this platform");
            }
            Err(e) => warn!(
                error = %e,
                "could not turn on jemalloc background threads; an arena's \
                 dirty pages are purged only when it is next used"
            ),
        }
    }
    let enabled = background_thread::read().unwrap_or(false);
    info!(
        background_thread = enabled,
        narenas = opt::narenas::read().ok(),
        dirty_decay_ms = read_decay_ms(b"opt.dirty_decay_ms\0"),
        muzzy_decay_ms = read_decay_ms(b"opt.muzzy_decay_ms\0"),
        malloc_conf = conf.as_deref(),
        "jemalloc options"
    );
    enabled
}

/// Whether a jemalloc option string (`key:value,key:value`) sets
/// `background_thread`, so the operator's choice wins over ours.
fn sets_background_thread(conf: &str) -> bool {
    conf.split(',')
        .filter_map(|option| option.split_once(':'))
        .any(|(key, _)| key == "background_thread")
}

/// Whether a mallctl failed with `ENOENT`, which `background_thread`
/// returns where jemalloc is built without background threads (macOS).
/// tikv-jemalloc-ctl's `Error` hides its errno but compares by it, and an
/// unknown mallctl name is `ENOENT`.
fn is_enoent(e: tikv_jemalloc_ctl::Error) -> bool {
    let mut mib = [0; 1];
    raw::name_to_mib(b"meteocore\0", &mut mib).err() == Some(e)
}

/// Reads a decay option in milliseconds (-1 = never purge).
fn read_decay_ms(name: &[u8]) -> Option<isize> {
    // SAFETY: both decay options are `ssize_t` in jemalloc (`ctl.c`), which
    // is `isize` on every target jemalloc is built for here.
    unsafe { raw::read::<isize>(name) }.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_background_thread_option_is_left_to_jemalloc() {
        assert!(sets_background_thread("background_thread:false"));
        assert!(sets_background_thread(
            "dirty_decay_ms:5000,background_thread:true"
        ));
        assert!(!sets_background_thread("max_background_threads:2"));
        assert!(!sets_background_thread("narenas:8"));
        assert!(!sets_background_thread(""));
    }

    /// The decay options read back at jemalloc's defaults: 10 s for dirty
    /// pages, muzzy pages purged at once.
    #[test]
    fn decay_options_read_back() {
        assert_eq!(read_decay_ms(b"opt.dirty_decay_ms\0"), Some(10_000));
        assert_eq!(read_decay_ms(b"opt.muzzy_decay_ms\0"), Some(0));
    }

    /// Only `ENOENT` counts as an unsupported platform: writing a read-only
    /// option (`EPERM`) does not, so it would be logged at WARN.
    #[test]
    fn only_enoent_is_unsupported() {
        let mut mib = [0; 1];
        let enoent = raw::name_to_mib(b"no_such_option\0", &mut mib).unwrap_err();
        assert!(is_enoent(enoent));
        let eperm = raw::write_str(b"opt.narenas\0", b"\0").unwrap_err();
        assert!(!is_enoent(eperm));
    }

    /// The acceptance check of #1310: after startup, background threads
    /// are on.
    #[cfg(target_os = "linux")]
    #[test]
    fn background_threads_are_on_after_configure() {
        assert!(configure());
        assert!(background_thread::read().unwrap());
    }

    /// jemalloc is built without background threads on macOS: the write
    /// fails with `ENOENT`, logged at INFO, and startup carries on without
    /// them.
    #[cfg(target_os = "macos")]
    #[test]
    fn background_threads_unsupported_on_macos_is_not_fatal() {
        assert!(is_enoent(background_thread::write(true).unwrap_err()));
        assert!(!configure());
    }
}
