//! Peak heap allocation of ODIM decodes (#1316), counted by a global
//! allocator on the decoding thread.
//!
//! A decode holds what it returns plus, transiently, a chunk or a band of
//! rows. hdf5-reader's own caches would hold a second copy of everything
//! decoded until the file drops, outside every budget: its 64 MiB chunk
//! cache, and the copy of each chunked dataset of at most 32 MiB that
//! `read_array` keeps on the file. The `committed_*` tests pin that on the
//! committed DMI composite, read as a composite and as a batch of moments;
//! `decode_peak_allocation` (ignored) reports the numbers on the uncommitted
//! radar fixtures:
//!
//! ```text
//! cargo test -p engine-odim --test decode_memory -- --ignored --nocapture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};

use ds_storage::bytes::Bytes;
use engine_odim::pvol::{read_moments_pixels, read_polar_volume};
use engine_odim::reader::{read_composite, RawPixels};

/// Counts the bytes live and their peak, per thread, so tests running in
/// parallel do not see each other's allocations.
struct Counting;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn track(delta: isize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            track(layout.size() as isize);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            track(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        track(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            track(new_size as isize - layout.size() as isize);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// `f`'s result and the most bytes it had allocated at once on this thread.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let start = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let out = f();
    let peak = PEAK.with(Cell::get) - start;
    (out, peak.max(0) as usize)
}

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata")
}

/// The first `.h5` file of `testdata/<dir>`, if present.
fn first_h5(dir: &str) -> Option<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(testdata().join(dir))
        .ok()?
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "h5"))
        .collect();
    paths.sort();
    paths.into_iter().next()
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// FNV-1a over a decoded array's type, shape and element bits: one number
/// to compare decodes across builds.
fn digest(px: &RawPixels) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    let (rows, cols) = px.shape();
    eat(&rows.to_le_bytes());
    eat(&cols.to_le_bytes());
    match px {
        RawPixels::U8(a) => {
            eat(b"u8");
            a.iter().for_each(|v| eat(&v.to_le_bytes()));
        }
        RawPixels::U16(a) => {
            eat(b"u16");
            a.iter().for_each(|v| eat(&v.to_le_bytes()));
        }
        RawPixels::I16(a) => {
            eat(b"i16");
            a.iter().for_each(|v| eat(&v.to_le_bytes()));
        }
        RawPixels::F32(a) => {
            eat(b"f32");
            a.iter().for_each(|v| eat(&v.to_bits().to_le_bytes()));
        }
    }
    h
}

/// A composite decode allocates what it returns and little more: no chunk
/// cache, no copy of the dataset kept on the file. The committed DMI
/// composite is 1728 × 1984 u8 in 128 deflated chunks of 108 × 248.
#[test]
fn committed_composite_decode_holds_no_second_copy() {
    let bytes = Bytes::from(std::fs::read(testdata().join("odim-dmi-fixture.h5")).unwrap());
    let (composite, peak) = peak_of(|| read_composite(&bytes).unwrap());
    let output = composite.size_bytes();
    assert_eq!(output, 1728 * 1984);
    assert!(
        peak < output + output / 4,
        "peak {peak} B for a {output} B composite: a second copy is held"
    );
}

/// The batch path a volume's moments decode through holds no second copy
/// either: the committed composite's dataset, decoded as a one-moment batch.
#[test]
fn committed_moment_batch_holds_no_second_copy() {
    let bytes = Bytes::from(std::fs::read(testdata().join("odim-dmi-fixture.h5")).unwrap());
    let (decoded, peak) =
        peak_of(|| read_moments_pixels(&bytes, [("/dataset1/data1/data", 1728, 1984)]).unwrap());
    let output: usize = decoded.iter().map(|(_, px)| px.size_bytes()).sum();
    assert_eq!(output, 1728 * 1984);
    assert!(
        peak < output + output / 4,
        "peak {peak} B for a {output} B moment: a second copy is held"
    );
}

/// Peak allocation of one whole-volume batch and one composite decode per
/// uncommitted radar fixture, with a digest of every decoded array.
#[test]
#[ignore = "benchmark: reads the uncommitted radar fixtures"]
fn decode_peak_allocation() {
    for dir in ["radar-fmi-pvol", "radar-smhi-pvol", "radar-dmi-pvol"] {
        let Some(path) = first_h5(dir) else {
            eprintln!("skipping {dir}: fixture absent");
            continue;
        };
        let bytes = Bytes::from(std::fs::read(&path).unwrap());
        let volume = read_polar_volume(&bytes).unwrap();
        let requests: Vec<_> = volume
            .sweeps
            .iter()
            .flat_map(|s| {
                s.moments
                    .iter()
                    .map(move |m| (m.dataset_path.as_str(), s.nrays, s.nbins))
            })
            .collect();
        let (decoded, peak) = peak_of(|| read_moments_pixels(&bytes, requests.clone()).unwrap());
        assert_eq!(decoded.len(), requests.len(), "{dir}: every moment decodes");
        let output: usize = decoded.iter().map(|(_, px)| px.size_bytes()).sum();
        let mut h: u64 = 0;
        for (path, px) in &decoded {
            h = h.rotate_left(7) ^ digest(px) ^ path.len() as u64;
        }
        println!(
            "{dir}: {} moments, batch peak {:.1} MiB for {:.1} MiB decoded, overhead {:.1} MiB, digest {h:016x}",
            decoded.len(),
            mib(peak),
            mib(output),
            mib(peak.saturating_sub(output)),
        );
    }

    for dir in ["radar-opera", "radar-dwd", "radar-dmi", "radar-smhi"] {
        let Some(path) = first_h5(dir) else {
            eprintln!("skipping {dir}: fixture absent");
            continue;
        };
        let bytes = Bytes::from(std::fs::read(&path).unwrap());
        let (composite, peak) = peak_of(|| read_composite(&bytes).unwrap());
        let output = composite.size_bytes();
        println!(
            "{dir}: composite {:?}, peak {:.1} MiB for {:.1} MiB decoded, overhead {:.1} MiB, digest {:016x}",
            composite.pixels.shape(),
            mib(peak),
            mib(output),
            mib(peak.saturating_sub(output)),
            digest(&composite.pixels),
        );
    }
}
