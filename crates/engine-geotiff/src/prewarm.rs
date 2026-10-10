//! Pre-warm newly published remote COG frames on the poll runtime (#1004).
//!
//! A projected WMS view renders as a sequential loop of 256 px meta-tiles,
//! one `get_raster_tile` each. On a cold frame every meta-tile that reaches
//! a source tile no earlier one fetched waits for its own storage round
//! trip, so the first view of a new frame costs about that many round trips
//! one after another (`docs/performance/cog-first-frame.md`). The poll that
//! discovers a frame therefore reads its encoded tiles into the engine's
//! compressed [`TileCache`], coarsest level first and within a per-frame
//! byte cap, and the first view only decodes.
//!
//! Only the compressed cache is filled. Decoding a whole OPERA frame would
//! put about 100 MB per poll into the decoded-chunk cache that every GeoTIFF
//! collection shares, evicting their chunks, while the request path decodes
//! the few tiles a view touches in milliseconds.
//!
//! Reads are coalesced with the request path's planner
//! ([`range_batch::plan`]) and run on blocking-pool threads of the poll
//! runtime through the storage bridges with an explicit handle (root
//! Critical Rule 7), [`CONCURRENCY`] at a time. Each read holds a background
//! reservation of the decode budget, which never takes the half of it that
//! requests keep. A read that finds requests holding more waits for them, up
//! to [`BUSY_WAIT`], and is then left to the request path: a burst of
//! renders must not make the poll skip the frame it is warming for them.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;

use crate::cache::TileCache;
use crate::decode_budget::{Budget, BUDGET};
use crate::range_batch;
use crate::reader::{self, DataSource, RemoteTileInfo, TiffMetadata};

/// Coalesced reads in flight per pre-warm. I/O bound, and kept below the
/// request path's fetch pool so a pre-warm never competes with renders for
/// the store's connections.
const CONCURRENCY: usize = 4;

/// Longest one read waits for requests to release the decode budget's
/// background half before it is left to the request path. Renders hold their
/// reservations for milliseconds, so this absorbs a burst; a budget saturated
/// for longer than this is reported, not waited out for the whole
/// time budget.
const BUSY_WAIT: Duration = Duration::from_secs(2);
/// First and longest pause between admission attempts while requests hold
/// the decode budget.
const BUSY_BACKOFF: (Duration, Duration) = (Duration::from_millis(5), Duration::from_millis(100));

/// Default `MC_COG_PREWARM_MB`.
const DEFAULT_FRAME_CAP_MB: usize = 32;

/// The most encoded bytes one frame pre-warms: `MC_COG_PREWARM_MB` (default
/// 32 MiB; 0 turns the pre-warm off), and at most an eighth of the engine's
/// tile cache, so the newest frames fit side by side and one large file
/// cannot evict the rest. Levels are taken coarsest first while the frame's
/// running total stays within it, so a file too large for the cap still gets
/// its overviews.
pub(crate) fn frame_cap(tile_cache_capacity: u64) -> usize {
    static CAP: LazyLock<usize> =
        LazyLock::new(|| parse_frame_cap(std::env::var("MC_COG_PREWARM_MB").ok().as_deref()));
    (*CAP).min(usize::try_from(tile_cache_capacity / 8).unwrap_or(usize::MAX))
}

/// `MC_COG_PREWARM_MB` in bytes; unset or unparsable is the default.
fn parse_frame_cap(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_FRAME_CAP_MB)
        .saturating_mul(1024 * 1024)
}

/// One coalesced read: tiles of one IFD that lie next to each other in the
/// file, each with its absolute byte range.
struct Read {
    ifd: u16,
    range: Range<usize>,
    tiles: Vec<(u32, Range<usize>)>,
}

/// What one frame still needs from storage.
pub(crate) struct FramePlan {
    /// The catalog entry's path: the tile cache key every render uses.
    path: PathBuf,
    source: Arc<DataSource>,
    reads: Vec<Read>,
    /// Finest levels left out because they would pass the cap.
    capped_levels: usize,
}

impl FramePlan {
    /// Nothing to fetch: every planned tile is already cached.
    pub(crate) fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn read_count(&self) -> usize {
        self.reads.len()
    }
}

/// Plan the pre-warm of one catalog entry: every level, coarsest first, whose
/// encoded tiles keep the frame within `cap`, and of those the tiles `cache`
/// does not hold. `None` for a source with nothing to fetch (local, in
/// memory) or with a tile layout the request path would reject too.
pub(crate) fn plan_frame(
    path: &Path,
    metadata: &TiffMetadata,
    source: &Arc<DataSource>,
    cache: &TileCache,
    cap: usize,
) -> Option<FramePlan> {
    let full = match source.as_ref() {
        DataSource::Remote { tile_info, .. } | DataSource::HttpDirect { tile_info, .. } => {
            tile_info
        }
        DataSource::LocalFile { .. } | DataSource::InMemory(_) => return None,
    };
    // Overviews are listed finest first; the full resolution (IFD 0) is the
    // finest of all.
    let levels: Vec<(usize, Option<&RemoteTileInfo>, u32, u32)> = metadata
        .overviews
        .iter()
        .rev()
        .map(|ov| {
            (
                ov.ifd_index,
                ov.tile_info.as_ref(),
                ov.tiles_across,
                ov.tiles_down,
            )
        })
        .chain(std::iter::once((
            0,
            Some(full),
            metadata.tiles_across,
            metadata.tiles_down,
        )))
        .collect();

    let mut plan = FramePlan {
        path: path.to_path_buf(),
        source: Arc::clone(source),
        reads: Vec::new(),
        capped_levels: 0,
    };
    let mut total = 0usize;
    for (level, &(ifd, info, across, down)) in levels.iter().enumerate() {
        // An overview without tile info (a header read too short to reach
        // its IFD) cannot be read remotely by requests either.
        let Some(info) = info else { continue };
        let ifd = u16::try_from(ifd).ok()?;
        let chunks = (across as usize)
            .saturating_mul(down as usize)
            .min(info.tile_offsets.len());
        let mut tiles: Vec<(u32, Range<usize>)> = Vec::with_capacity(chunks);
        let mut bytes = 0usize;
        for chunk in 0..chunks {
            let (_, _, offset, count) =
                reader::remote_chunk_layout(info, metadata.samples_per_pixel, chunk).ok()?;
            if count == 0 {
                continue; // an empty tile reads as nodata without I/O
            }
            bytes = bytes.saturating_add(count);
            tiles.push((u32::try_from(chunk).ok()?, offset..offset + count));
        }
        if total.saturating_add(bytes) > cap {
            plan.capped_levels = levels.len() - level;
            break;
        }
        total += bytes;
        let misses: Vec<(usize, Range<usize>)> = tiles
            .iter()
            .enumerate()
            .filter(|(_, (chunk, _))| !cache.contains_untracked(path, *chunk, ifd))
            .map(|(i, (_, range))| (i, range.clone()))
            .collect();
        for batch in range_batch::plan(misses, range_batch::MAX_TILES) {
            plan.reads.push(Read {
                ifd,
                range: batch.range,
                tiles: batch.tiles.iter().map(|&i| tiles[i].clone()).collect(),
            });
        }
    }
    Some(plan)
}

/// What one pre-warm did, for its log line.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    pub frames: usize,
    pub reads: usize,
    pub tiles: usize,
    pub bytes: usize,
    /// Reads that failed; their tiles load on first view.
    pub failed: usize,
    /// Reads left to requests because these held the decode budget's
    /// background half for longer than [`BUSY_WAIT`].
    pub busy: usize,
    /// Reads skipped because the engine was shutting down, or because one
    /// read is larger than the decode budget's background half.
    pub deferred: usize,
    /// Reads cut off by the time budget.
    pub unfinished: usize,
    /// Finest levels left out of a frame by the per-frame cap, summed.
    pub capped_levels: usize,
    pub first_error: Option<String>,
}

/// The result of one coalesced read.
enum ReadResult {
    Warmed { tiles: usize, bytes: usize },
    Failed(String),
    Busy,
    Deferred,
}

/// Fetch every planned read into `cache`, [`CONCURRENCY`] at a time, for at
/// most `budget`, each charged to the process-wide decode budget. `stop` is
/// checked before each read starts and while it waits for the decode budget
/// (engine shutdown). Must run inside a multi-thread Tokio runtime: the reads
/// go to its blocking pool.
pub(crate) async fn warm(
    plans: &[FramePlan],
    cache: &TileCache,
    budget: Duration,
    stop: impl Fn() -> bool,
) -> Outcome {
    warm_with(plans, cache, &BUDGET, BUSY_WAIT, budget, stop).await
}

/// [`warm`] against the decode budget `decode`, waiting at most `busy_wait`
/// per read for requests to leave room in it.
pub(crate) async fn warm_with(
    plans: &[FramePlan],
    cache: &TileCache,
    decode: &Arc<Budget>,
    busy_wait: Duration,
    budget: Duration,
    stop: impl Fn() -> bool,
) -> Outcome {
    let handle = tokio::runtime::Handle::current();
    let mut outcome = Outcome {
        frames: plans.len(),
        capped_levels: plans.iter().map(|p| p.capped_levels).sum(),
        ..Outcome::default()
    };
    // Indices, not references: a stream closure over borrowed items is not
    // `Send` for every lifetime, and the poll loop's future must be.
    let jobs: Vec<(usize, usize)> = plans
        .iter()
        .enumerate()
        .flat_map(|(p, plan)| (0..plan.reads.len()).map(move |r| (p, r)))
        .collect();
    let planned = jobs.len();
    let mut done = 0usize;
    let stop = &stop;
    let results = futures::stream::iter(jobs)
        .map(|(p, r)| {
            let skip = stop();
            let handle = handle.clone();
            let plan = &plans[p];
            async move {
                if skip {
                    return ReadResult::Deferred;
                }
                warm_read(plan, &plan.reads[r], cache, decode, busy_wait, handle, stop).await
            }
        })
        .buffer_unordered(CONCURRENCY)
        .take_until(tokio::time::sleep(budget));
    let mut results = std::pin::pin!(results);
    while let Some(result) = results.next().await {
        done += 1;
        match result {
            ReadResult::Warmed { tiles, bytes } => {
                outcome.reads += 1;
                outcome.tiles += tiles;
                outcome.bytes += bytes;
            }
            ReadResult::Failed(error) => {
                outcome.failed += 1;
                outcome.first_error.get_or_insert(error);
            }
            ReadResult::Busy => outcome.busy += 1,
            ReadResult::Deferred => outcome.deferred += 1,
        }
    }
    outcome.unfinished = planned - done;
    outcome
}

/// Fetch one coalesced read and insert each of its tiles.
async fn warm_read(
    plan: &FramePlan,
    read: &Read,
    cache: &TileCache,
    decode: &Arc<Budget>,
    busy_wait: Duration,
    handle: tokio::runtime::Handle,
    stop: &impl Fn() -> bool,
) -> ReadResult {
    // The read's input and the per-tile copies coexist until it is cached.
    let reservation = read.range.len().saturating_mul(2);
    if reservation > decode.background_limit() {
        return ReadResult::Deferred; // never fits, so do not wait for it
    }
    let waiting = tokio::time::Instant::now();
    let mut pause = BUSY_BACKOFF.0;
    let permit = loop {
        if let Some(permit) = decode.try_reserve_background(reservation) {
            break permit;
        }
        if stop() {
            return ReadResult::Deferred;
        }
        if waiting.elapsed() >= busy_wait {
            return ReadResult::Busy;
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(BUSY_BACKOFF.1);
    };
    let source = Arc::clone(&plan.source);
    let range = read.range.clone();
    // The permit travels with the read, so a read the time budget abandons
    // stays charged until its blocking fetch actually finishes.
    let fetched = tokio::task::spawn_blocking(move || {
        let bytes = reader::fetch_encoded_range(&source, range, &handle);
        (bytes, permit)
    })
    .await;
    let (bytes, _permit) = match fetched {
        Ok((Ok(bytes), permit)) => (bytes, permit),
        Ok((Err(e), _)) => return ReadResult::Failed(e.to_string()),
        Err(e) => return ReadResult::Failed(format!("pre-warm read task failed: {e}")),
    };
    if bytes.len() != read.range.len() {
        return ReadResult::Failed(format!(
            "short range read: {} of {} bytes",
            bytes.len(),
            read.range.len()
        ));
    }
    let mut payload = 0;
    for (chunk, range) in &read.tiles {
        let start = range.start - read.range.start;
        // Copy only this tile: a slice would keep the whole read alive in the
        // LRU while its weigher charges the slice.
        let tile = Bytes::copy_from_slice(&bytes[start..start + range.len()]);
        payload += tile.len();
        cache.insert(&plan.path, *chunk, read.ifd, tile);
    }
    ReadResult::Warmed {
        tiles: read.tiles.len(),
        bytes: payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_cap_parses_megabytes_and_zero_disables() {
        assert_eq!(parse_frame_cap(None), DEFAULT_FRAME_CAP_MB * 1024 * 1024);
        assert_eq!(parse_frame_cap(Some(" 8 ")), 8 * 1024 * 1024);
        assert_eq!(parse_frame_cap(Some("0")), 0);
        assert_eq!(
            parse_frame_cap(Some("lots")),
            DEFAULT_FRAME_CAP_MB * 1024 * 1024
        );
    }
}
