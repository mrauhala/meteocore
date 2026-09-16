//! Conservative planning for nearby cache-missing COG tiles. No I/O here.
use std::ops::Range;

const MAX_SPAN: usize = 1024 * 1024;
const MAX_GAP: usize = 4096;
const MAX_TILES: usize = 16;

/// Opt-in: current OPERA measurements do not justify enabling it universally.
pub(crate) fn limit() -> usize {
    static LIMIT: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("MC_COG_RANGE_BATCH_TILES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, MAX_TILES)
    });
    *LIMIT
}

pub(crate) struct Batch {
    pub range: Range<usize>,
    pub tiles: Vec<usize>,
    payload: usize,
}

/// Inputs are validated, nonempty ranges. Sort by file offset (not tile index).
/// Merge only disjoint ranges: malformed overlapping tile layouts stay separate.
/// Gaps together may add at most 10% over the useful compressed payload.
pub(crate) fn plan(mut ranges: Vec<(usize, Range<usize>)>, limit: usize) -> Vec<Batch> {
    let limit = limit.clamp(1, MAX_TILES);
    if limit > 1 {
        ranges.sort_unstable_by_key(|(_, r)| r.start);
    }
    let mut batches: Vec<Batch> = Vec::new();
    for (tile, range) in ranges {
        if let Some(last) = batches.last_mut() {
            let payload = last.payload.saturating_add(range.len());
            let span = range.end.saturating_sub(last.range.start);
            if range.start >= last.range.end
                && range.start - last.range.end <= MAX_GAP
                && span <= MAX_SPAN
                && span.saturating_sub(payload) <= payload / 10
                && last.tiles.len() < limit
            {
                last.range.end = range.end;
                last.payload = payload;
                last.tiles.push(tile);
                continue;
            }
        }
        batches.push(Batch {
            payload: range.len(),
            range,
            tiles: vec![tile],
        });
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearby_offsets_merge_in_file_order_with_bounded_overfetch() {
        let batches = plan(vec![(3, 220..320), (1, 0..100), (2, 110..210)], 4);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].range, 0..320);
        assert_eq!(batches[0].tiles, [1, 2, 3]);
    }

    #[test]
    fn disabled_keeps_individual_ranges_in_input_order() {
        let batches = plan(vec![(1, 10..20), (0, 0..10)], 1);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].tiles, [1]);
    }

    #[test]
    fn sparse_overlapping_large_and_many_ranges_stay_bounded() {
        for ranges in [
            vec![(0, 0..10), (1, 20..30)], // 10-byte gap > 10% payload
            vec![(0, 0..100_000), (1, 105_000..200_000)], // gap > 4 KiB
            vec![(0, 0..800_000), (1, 800_000..1_100_000)], // span > 1 MiB
            vec![(0, 0..100), (1, 50..150)], // overlap
        ] {
            assert_eq!(plan(ranges, 4).len(), 2);
        }
        let batches = plan((0..100).map(|i| (i, i * 100..(i + 1) * 100)).collect(), 100);
        assert_eq!(batches.len(), 7);
        assert!(batches.iter().all(|b| b.tiles.len() <= MAX_TILES));
        // A single otherwise-valid large tile still uses its ordinary read.
        assert_eq!(
            plan(vec![(0, 0..2 * MAX_SPAN)], 4)[0].range.len(),
            2 * MAX_SPAN
        );
    }
}
