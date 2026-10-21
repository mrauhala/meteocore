//! Late reports for analysis times older than the `precompute` window
//! (#1206).
//!
//! The poll re-reads the latest `precompute` times every cycle. An older
//! time's grid stays cached under the revision it was computed at, so a
//! late or corrected report for it would be served stale until the grid is
//! evicted. After the pre-compute, each poll cycle therefore re-reads at
//! most [`RECHECK_PER_CYCLE`] of the older times whose grid the cache still
//! holds, round-robin from the newest (Critical Rule 9: a cap per cycle,
//! never one read per cached grid). A changed fingerprint is a revision as
//! anywhere else (`Inner::observe`): it moves that time's revision and the
//! content version, and the new grid is computed ahead with background
//! admission. An unchanged one leaves the cached grid alone; the check
//! never bumps its recency, so it still ages out of the cache.
//!
//! No request reads the source to find a revision, and an older time whose
//! grid is not cached needs no check: its next request reads the source.
//!
//! Worst case: with K cached grids older than the window, each is re-read
//! once in every ⌈K / [`RECHECK_PER_CYCLE`]⌉ poll cycles, so a revision of
//! one is found within that many poll intervals, then computed. K is at
//! most the grids the cache holds for the collection: a day of hourly
//! grids of two parameters with the default `precompute` (K = 42) is
//! re-read every 6 cycles, 6 minutes at the default 60 s poll.

use std::cmp::Reverse;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::Inner;
use crate::compute::{GridKey, CACHE};

/// Older analyses the poll re-reads per cycle, across the parameters.
pub const RECHECK_PER_CYCLE: usize = 8;

/// A (time, parameter) in the order the re-checks visit them: the newest
/// time first, then by parameter.
pub(super) type Position = (Reverse<DateTime<Utc>>, usize);

/// The next round of a round-robin over `candidates`: at most `n` of them,
/// in order, from the first after `cursor`, wrapping round; `cursor` moves
/// to the last one taken. While the set holds still, each candidate is
/// taken once in every ⌈len / n⌉ rounds.
pub(super) fn next_round<T: Ord + Copy>(
    mut candidates: Vec<T>,
    cursor: &mut Option<T>,
    n: usize,
) -> Vec<T> {
    candidates.sort_unstable();
    candidates.dedup();
    let start = cursor.map_or(0, |c| candidates.partition_point(|x| *x <= c));
    let (after, before) = (&candidates[start..], &candidates[..start]);
    let round: Vec<T> = after.iter().chain(before).take(n).copied().collect();
    if let Some(&last) = round.last() {
        *cursor = Some(last);
    }
    round
}

impl Inner {
    /// One round of re-checks of the older analyses the cache holds (module
    /// docs). `times` is the axis the cycle pre-computed from.
    pub(super) fn recheck_older(self: &Arc<Self>, times: &[DateTime<Utc>]) {
        // The latest `precompute` times were just re-read.
        let Some(&newest_older) = times
            .len()
            .checked_sub(self.precompute + 1)
            .and_then(|i| times.get(i))
        else {
            return;
        };
        let recorded: Vec<(GridKey, Position)> = self
            .inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|((_, time), _)| *time <= newest_older)
            .map(|(&(parameter, time), input)| {
                let key = GridKey {
                    epoch: self.epoch,
                    parameter,
                    time: time.timestamp(),
                    revision: input.revision,
                };
                (key, (Reverse(time), parameter))
            })
            .collect();
        // `contains_key` does not bump the grid's recency.
        let cached: Vec<Position> = recorded
            .into_iter()
            .filter(|(key, _)| CACHE.contains_key(key))
            .map(|(_, position)| position)
            .collect();
        let round = {
            let mut cursor = self.recheck_from.lock().unwrap_or_else(|e| e.into_inner());
            next_round(cached, &mut cursor, RECHECK_PER_CYCLE)
        };
        for (Reverse(time), parameter) in round {
            if self.stopped.load(Ordering::Relaxed) {
                return;
            }
            self.recheck_one(parameter, time);
        }
    }

    /// Re-read one older analysis; a revision moves its revision and the
    /// content version, and computes the new grid ahead.
    fn recheck_one(self: &Arc<Self>, parameter: usize, time: DateTime<Utc>) {
        let before = self.key(parameter, time);
        let Some(slice) = self.read_ahead(parameter, time) else {
            return;
        };
        self.observe(parameter, time, slice.fingerprint);
        if self.key(parameter, time) != before {
            self.compute_ahead(parameter, time, slice);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rounds walk the candidates in order from the cursor and wrap round;
    /// a candidate added behind the cursor is reached on the next lap.
    #[test]
    fn rounds_are_round_robin() {
        let mut cursor = None;
        let all: Vec<u32> = (0..5).collect();
        assert_eq!(next_round(all.clone(), &mut cursor, 2), [0, 1]);
        assert_eq!(next_round(all.clone(), &mut cursor, 2), [2, 3]);
        assert_eq!(next_round(all.clone(), &mut cursor, 2), [4, 0]);
        assert_eq!(cursor, Some(0));
        // Unordered and repeated input; the cursor need not be a candidate.
        cursor = Some(2);
        assert_eq!(next_round(vec![9, 1, 3, 1], &mut cursor, 2), [3, 9]);
        assert_eq!(next_round(vec![9, 1, 3], &mut cursor, 2), [1, 3]);
        // Never more than there are, and nothing moves the cursor when
        // there is nothing to check.
        assert_eq!(next_round(vec![7], &mut cursor, 8), [7]);
        assert_eq!(next_round(Vec::new(), &mut cursor, 8), Vec::<u32>::new());
        assert_eq!(cursor, Some(7));
    }

    /// The visiting order is the newest time first.
    #[test]
    fn newest_first() {
        let t = |h: i64| DateTime::from_timestamp(h * 3600, 0).unwrap();
        let mut cursor = None;
        let round = next_round(
            vec![(Reverse(t(1)), 0), (Reverse(t(3)), 1), (Reverse(t(3)), 0)],
            &mut cursor,
            8,
        );
        assert_eq!(
            round,
            [(Reverse(t(3)), 0), (Reverse(t(3)), 1), (Reverse(t(1)), 0)]
        );
    }
}
