//! Heap the catalog's open file handles hold (#1067), with a counting
//! allocator: what one poll leaves allocated, divided by the files it opened.
//! Every catalogued file stays open with the metadata it was read through,
//! so this times the files a collection keeps (`max_runs` × files per run)
//! is resident.
//!
//! `cargo run -p engine-netcdf --example handle_memory [dir] [variable …]`
//! (default: the committed MEPS fixture, every variable).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use ds_core::config::NetcdfConfig;
use engine_netcdf::NetcdfEngine;

struct Counting;

static ALLOCATED: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        ALLOCATED.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/meps-netcdf").to_string()
    });
    let variables: Vec<String> = args.collect();
    let mut toml = format!("data_path = {dir:?}\nmax_files = 100000\n");
    if !variables.is_empty() {
        toml.push_str(&format!("variables = {variables:?}\n"));
    }
    let config: NetcdfConfig = toml::from_str(&toml).expect("config");
    let files = std::fs::read_dir(&dir)
        .expect("data directory")
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".nc"))
        })
        .count();
    let engine = NetcdfEngine::new("measure", &config).expect("engine");
    let before = ALLOCATED.load(Ordering::Relaxed);
    engine.poll_once();
    let held = ALLOCATED.load(Ordering::Relaxed) - before;
    let info = ds_core::map_engine::MapEngine::raster_info(&engine);
    println!(
        "{files} files, {} variables served, {} valid times: {held} bytes held, {} per file",
        info.parameters.len(),
        info.times.len(),
        held / files.max(1) as isize
    );
}
