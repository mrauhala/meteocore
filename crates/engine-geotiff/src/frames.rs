//! The frames of one EDR position or area query, each one file of the
//! query's window, read as jobs of `ds_executor::field_jobs::run_field_jobs`
//! (#1495): tasks of the query runtime, inside `block_in_place`, under the
//! query's deadline. A result is placed by its frame's index, so the `t`
//! axis keeps the catalog's order. A position query reads a few frames at a
//! time ([`position_frame_reads`]), an area query one ([`AREA_FRAME_READS`]).

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;

use crate::catalog::FileEntry;
use crate::reader::{DataSource, TiffMetadata};

/// The most frames one position query reads at once when every frame is a
/// COG an object store serves by range (#1495). A frame's read is one tile
/// range GET on the reading thread, mostly waiting on the store, so eight
/// overlap eight round trips: a 144-frame window pays 18 of them, not 144.
///
/// What eight hold of `decode_budget::BUDGET` (1 GiB by default): a frame
/// reserves its one tile's encoded, decompressed and native bytes (about
/// 2–3 MiB for a 512 × 512 Float32 tile), so eight hold about 24 MiB per
/// query, and api-edr's at most eight running queries about 192 MiB between
/// them. A refused reservation stays the query's 503.
pub(crate) const REMOTE_FRAME_READS: usize = 8;

/// The frames one area query reads at once: one, as before #1495. An area
/// frame's tiles are fetched on the process-wide `TILE_FETCH_POOL` (16
/// threads by default, `MC_COG_TILE_CONCURRENCY`), which the cold map
/// renders of every remote COG collection share. Eight area frames at once
/// would take its threads from them: four cold 24-frame area queries made
/// an unrelated one-frame read wait 415 ms instead of 218 ms, the convoy of
/// #240 and #243. A frame's tiles still overlap in the pool.
pub(crate) const AREA_FRAME_READS: usize = 1;

/// One frame a query reads: its place on the `t` axis, and what reading it
/// takes, owned, since the read runs as a task of its own.
pub(crate) struct Frame {
    pub(crate) index: usize,
    pub(crate) metadata: Arc<TiffMetadata>,
    pub(crate) source: Arc<DataSource>,
    pub(crate) path: PathBuf,
}

/// The frames of `entries` that can be read, by their index there. An
/// entry without metadata (a STAC stub its load left unloaded) reads
/// nothing and stays null.
pub(crate) fn frames(entries: &[(&DateTime<Utc>, &FileEntry)]) -> Vec<Frame> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, (_, entry))| {
            Some(Frame {
                index,
                metadata: entry.metadata()?.clone(),
                source: entry.source()?.clone(),
                path: entry.path.clone(),
            })
        })
        .collect()
}

/// How many of a position query's `frames` are read at once:
/// [`REMOTE_FRAME_READS`] when an object store serves every one, else one
/// at a time, as before #1495.
///
/// - A frame decoded in process (a local file through the page cache, or a
///   remote file the scan downloaded whole) has no round trip to overlap,
///   and its cold decode reserves its tile and the decoder's 64 MiB buffer
///   cap of the decode budget: eight would reserve about half of the
///   default 1 GiB per query, and a second such query would be refused.
/// - A STAC asset is read by direct HTTP range requests, which no host
///   limit or 429 back-off of `ds_storage` bounds: eight per query, eight
///   queries at once, would be 64 GETs on one asset host.
pub(crate) fn position_frame_reads(frames: &[Frame]) -> usize {
    if frames
        .iter()
        .all(|frame| frame.source.reads_from_object_store())
    {
        REMOTE_FRAME_READS
    } else {
        1
    }
}

/// Read `frames`, at most `concurrency` at once, with `read`, and hand each
/// frame's index and value to `place`, in completion order. On the first
/// error or once the deadline passes no read starts, and the reads running
/// finish before this returns: the error is the query's.
pub(crate) fn read_frames<T: Send + 'static>(
    concurrency: usize,
    frames: Vec<Frame>,
    read: impl Fn(&Frame) -> Result<T, DataServerError> + Send + Sync + 'static,
    mut place: impl FnMut(usize, T) + Send,
) -> Result<(), DataServerError> {
    ds_executor::field_jobs::run_field_jobs(
        ds_executor::query_runtime(),
        concurrency,
        frames.into_iter(),
        move |frame: Frame| read(&frame).map(|value| (frame.index, value)),
        |(index, value)| {
            place(index, value);
            Ok(())
        },
    )
}
