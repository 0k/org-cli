//! Cross-entry CLOCK overlap detection.
//!
//! Every other CLOCK rule in this crate is *per line*: give it one
//! `CLOCK:` string and it answers on its own (see [`crate::org::clock`]).
//! Overlap is the first rule whose unit is a **pair** of clock
//! entries drawn from anywhere in the corpus, so it lives in its own
//! module and reports its own record type.
//!
//! # Interval semantics
//!
//! Intervals are **half-open**: `[start, end)`. Two intervals overlap
//! iff they share at least one instant, i.e.
//!
//! ```text
//! a.start < b.end  &&  b.start < a.end
//! ```
//!
//! Consequences, all of them intentional:
//!
//! - **Adjacent clocks do not overlap.** `10:00–11:00` followed by
//!   `11:00–12:00` is the exact shape Emacs produces when you clock
//!   out of one task and into the next, so treating the shared
//!   boundary instant as a conflict would flag normal work.
//! - **Zero-length clocks never overlap anything.** `=> 0:00` spans
//!   no instant at all. They may well be a mistake, but that is a
//!   different rule's job to report.
//!
//! # Instants, not wall-clock text
//!
//! Comparison is done on UTC seconds, never on the rendered local
//! time. `[… 09:30 +0800]` and `[… 09:30 +0200]` are six hours
//! apart and must not be confused with each other; feeding this
//! module the values from [`crate::org::clock::ClockLine`] keeps
//! that conversion in one place.

use std::path::PathBuf;

/// One clock entry reduced to what overlap detection needs: an
/// instant range plus enough identity to point a human back at the
/// offending line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interval {
    /// Start instant, in UTC seconds.
    pub start: i64,
    /// End instant, in UTC seconds. Never less than `start` —
    /// `start-after-end` lines are rejected upstream by
    /// [`crate::org::clock::parse_clock_line`].
    pub end: i64,
    /// File the CLOCK line was read from.
    pub file: PathBuf,
    /// 1-based line number within `file`.
    pub line: usize,
    /// The original line, unmodified, including leading whitespace.
    pub raw: String,
}

/// Two intervals that share at least one instant.
///
/// `a` and `b` are indices into the slice passed to
/// [`find_overlaps`], **after** it has been sorted in place. `a` is
/// always the interval that sorts earlier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlapPair {
    /// Index of the earlier-sorting interval.
    pub a: usize,
    /// Index of the later-sorting interval.
    pub b: usize,
    /// Size of the shared span, in seconds. Always `> 0`.
    pub overlap_seconds: i64,
}

/// Sort `intervals` chronologically and report every overlapping
/// pair.
///
/// The slice is sorted **in place** by `(start, end)`; the indices
/// in the returned pairs refer to that sorted order.
///
/// # Algorithm
///
/// Sort, then sweep once while keeping a small set of intervals that
/// are still "open" at the current position. Because the input is
/// sorted by start, any interval whose `end <= cur.start` can never
/// meet a later interval and is dropped from the active set — that
/// pruning is what keeps the sweep linear in practice.
///
/// Cost is `O(n log n)` for the sort plus `O(n + p)` for the sweep,
/// where `p` is the number of pairs actually reported. A corpus in
/// which everything overlaps everything is inherently `O(n²)`,
/// because that many pairs genuinely exist.
///
/// Pair order is deterministic: pairs are emitted in sorted order of
/// their later element, and within one element in sorted order of
/// the earlier one.
pub fn find_overlaps(intervals: &mut [Interval]) -> Vec<OverlapPair> {
    intervals.sort_by_key(|i| (i.start, i.end));
    find_overlaps_sorted(intervals)
}

/// Report every overlapping pair in an **already sorted** slice.
///
/// This is the sweep without the sort. The cache hands over a list
/// that is sorted by construction, so re-sorting it would be wasted
/// work and, worse, would hide a cache that delivered unsorted data.
/// Sortedness is therefore asserted in debug builds rather than
/// repaired.
///
/// Semantics are identical to [`find_overlaps`]; see there.
pub fn find_overlaps_sorted(intervals: &[Interval]) -> Vec<OverlapPair> {
    debug_assert!(
        intervals
            .windows(2)
            .all(|w| (w[0].start, w[0].end) <= (w[1].start, w[1].end)),
        "find_overlaps_sorted requires (start, end)-sorted input"
    );

    let mut pairs = Vec::new();
    // Indices of intervals still open at the current sweep position,
    // kept in sorted order so emitted pairs stay deterministic.
    let mut active: Vec<usize> = Vec::new();

    for cur in 0..intervals.len() {
        let cur_start = intervals[cur].start;

        // Drop everything that closed at or before `cur` starts.
        // Half-open: touching endpoints (`end == cur_start`) do not
        // overlap, so `<=` is the correct comparison here.
        active.retain(|&idx| intervals[idx].end > cur_start);

        // A zero-length interval covers no instant: it neither
        // overlaps the active set nor can it be overlapped later.
        if intervals[cur].end == cur_start {
            continue;
        }

        for &idx in &active {
            // `idx` is active, so `intervals[idx].end > cur_start`,
            // and sorting guarantees `intervals[idx].start <=
            // cur_start`. The two therefore genuinely share a span.
            let shared_end = intervals[idx].end.min(intervals[cur].end);
            let overlap_seconds = shared_end - cur_start;
            debug_assert!(
                overlap_seconds > 0,
                "active set must only hold genuinely-overlapping intervals"
            );
            pairs.push(OverlapPair {
                a: idx,
                b: cur,
                overlap_seconds,
            });
        }

        active.push(cur);
    }

    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an interval from bare seconds; file/line/raw are noise
    /// for the algorithm itself and get a stable placeholder.
    fn iv(start: i64, end: i64) -> Interval {
        Interval {
            start,
            end,
            file: PathBuf::from("/t.org"),
            line: 1,
            raw: String::new(),
        }
    }

    fn iv_at(start: i64, end: i64, file: &str, line: usize) -> Interval {
        Interval {
            start,
            end,
            file: PathBuf::from(file),
            line,
            raw: String::new(),
        }
    }

    const H: i64 = 3600;

    #[test]
    fn test_empty_input_has_no_overlaps() {
        let mut v: Vec<Interval> = Vec::new();
        assert!(find_overlaps(&mut v).is_empty());
    }

    #[test]
    fn test_single_interval_has_no_overlaps() {
        let mut v = vec![iv(0, H)];
        assert!(find_overlaps(&mut v).is_empty());
    }

    #[test]
    fn test_adjacent_intervals_do_not_overlap() {
        // 10:00-11:00 then 11:00-12:00 — the exact shape Emacs
        // produces on clock-out/clock-in. Must stay clean.
        let mut v = vec![iv(10 * H, 11 * H), iv(11 * H, 12 * H)];
        assert!(
            find_overlaps(&mut v).is_empty(),
            "half-open intervals must tolerate a shared boundary instant"
        );
    }

    #[test]
    fn test_disjoint_intervals_do_not_overlap() {
        let mut v = vec![iv(0, H), iv(5 * H, 6 * H)];
        assert!(find_overlaps(&mut v).is_empty());
    }

    #[test]
    fn test_partial_overlap_is_reported_with_shared_span() {
        // 10:00-12:00 and 11:00-13:00 share exactly one hour.
        let mut v = vec![iv(10 * H, 12 * H), iv(11 * H, 13 * H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].overlap_seconds, H);
        assert_eq!((pairs[0].a, pairs[0].b), (0, 1));
    }

    #[test]
    fn test_full_containment_is_reported() {
        // 09:00-18:00 fully contains 12:00-13:00.
        let mut v = vec![iv(9 * H, 18 * H), iv(12 * H, 13 * H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].overlap_seconds, H, "shared span is the inner one");
    }

    #[test]
    fn test_identical_intervals_are_reported() {
        let mut v = vec![iv(10 * H, 11 * H), iv(10 * H, 11 * H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].overlap_seconds, H);
    }

    #[test]
    fn test_zero_length_interval_never_overlaps() {
        // A `=> 0:00` clock sitting inside a long one covers no
        // instant, so it is not a conflict.
        let mut v = vec![iv(9 * H, 18 * H), iv(12 * H, 12 * H)];
        assert!(find_overlaps(&mut v).is_empty());
    }

    #[test]
    fn test_two_zero_length_intervals_at_same_instant_do_not_overlap() {
        let mut v = vec![iv(12 * H, 12 * H), iv(12 * H, 12 * H)];
        assert!(find_overlaps(&mut v).is_empty());
    }

    #[test]
    fn test_zero_length_at_boundary_does_not_overlap() {
        // Degenerate clock exactly at the end of another one.
        let mut v = vec![iv(9 * H, 12 * H), iv(12 * H, 12 * H)];
        assert!(find_overlaps(&mut v).is_empty());
    }

    #[test]
    fn test_three_way_overlap_reports_every_pair() {
        // All three cover 12:00-12:30 → 3 distinct pairs.
        let mut v = vec![iv(10 * H, 13 * H), iv(11 * H, 14 * H), iv(12 * H, 15 * H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 3, "got {:?}", pairs);
    }

    #[test]
    fn test_chain_reports_only_genuine_neighbours() {
        // A-B overlap, B-C overlap, but A and C are disjoint.
        let mut v = vec![iv(0, 2 * H), iv(H, 3 * H), iv(2 * H + 1, 4 * H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 2, "A and C must not be paired: {:?}", pairs);
    }

    #[test]
    fn test_long_interval_still_overlaps_much_later_one() {
        // A multi-day interval must stay active across many
        // shorter ones that start well after it.
        let day = 24 * H;
        let mut v = vec![iv(0, 5 * day), iv(day, day + H), iv(4 * day, 4 * day + H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn test_pruning_does_not_lose_pairs_when_earlier_ends_sooner() {
        // Sorted by start, but the FIRST interval ends before the
        // second one does: pruning must key on `end`, not position.
        let mut v = vec![iv(0, 10 * H), iv(H, 2 * H), iv(3 * H, 4 * H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 2, "the long opener overlaps both: {:?}", pairs);
    }

    #[test]
    fn test_cross_file_overlap_is_reported() {
        let mut v = vec![
            iv_at(10 * H, 12 * H, "/a.org", 4),
            iv_at(11 * H, 13 * H, "/b.org", 9),
        ];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 1);
        let (a, b) = (&v[pairs[0].a], &v[pairs[0].b]);
        assert_eq!(a.file, PathBuf::from("/a.org"));
        assert_eq!(a.line, 4);
        assert_eq!(b.file, PathBuf::from("/b.org"));
        assert_eq!(b.line, 9);
    }

    #[test]
    fn test_input_is_sorted_in_place() {
        let mut v = vec![iv(5 * H, 6 * H), iv(H, 2 * H), iv(3 * H, 4 * H)];
        find_overlaps(&mut v);
        let starts: Vec<i64> = v.iter().map(|i| i.start).collect();
        assert_eq!(starts, vec![H, 3 * H, 5 * H]);
    }

    #[test]
    fn test_pair_indices_point_at_sorted_positions() {
        // Feed them out of order; indices must address the sorted
        // slice, with `a` the earlier one.
        let mut v = vec![iv(11 * H, 13 * H), iv(10 * H, 12 * H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 1);
        assert_eq!(v[pairs[0].a].start, 10 * H);
        assert_eq!(v[pairs[0].b].start, 11 * H);
    }

    #[test]
    fn test_overlap_uses_instants_not_wall_clock() {
        // Same rendered local time, six hours apart in UTC:
        // 09:30+0800 == 01:30Z, 09:30+0200 == 07:30Z. A one-hour
        // clock at each must NOT be reported as overlapping.
        let a_start = H + 30 * 60; // 01:30Z
        let b_start = 7 * H + 30 * 60; // 07:30Z
        let mut v = vec![iv(a_start, a_start + H), iv(b_start, b_start + H)];
        assert!(find_overlaps(&mut v).is_empty());
    }

    #[test]
    fn test_many_identical_intervals_report_every_pair() {
        // n intervals all covering the same span → n*(n-1)/2 pairs.
        let mut v: Vec<Interval> = (0..5).map(|_| iv(10 * H, 11 * H)).collect();
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 10);
    }

    #[test]
    fn test_negative_instants_are_handled() {
        // Pre-epoch dates exist in the corpus (a 1978 CLOCK line).
        let mut v = vec![iv(-10 * H, -8 * H), iv(-9 * H, -7 * H)];
        let pairs = find_overlaps(&mut v);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].overlap_seconds, H);
    }

    // O2
    #[test]
    fn sorted_entry_matches_sorting_entry() {
        let day = 24 * H;
        let mut a = vec![
            iv(11 * H, 14 * H),
            iv(0, 5 * day),
            iv(10 * H, 13 * H),
            iv(12 * H, 12 * H),
            iv(12 * H, 15 * H),
            iv(4 * day, 4 * day + H),
        ];
        let via_sort = find_overlaps(&mut a);
        // `a` is now sorted; the sorted entry point must agree exactly.
        let via_sorted = find_overlaps_sorted(&a);
        assert_eq!(via_sort, via_sorted);
    }
}
