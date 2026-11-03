use crate::results::SimulationResults;
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

/// Order-statistics structure over the set of currently "missing" sequence
/// numbers, used to count inversions online for the Kendall tau distance.
///
/// It supports, in `O(log W)` where `W` is the current reordering window:
///   * [`insert_range`](MissingRankTree::insert_range) of a contiguous block of
///     numbers (the gap opened up by a new high-water arrival),
///   * [`remove`](MissingRankTree::remove) of a single number (a late arrival),
///   * [`rank_below`](MissingRankTree::rank_below) — how many stored numbers are
///     strictly less than a given one.
///
/// # Fenwick (binary-indexed) tree primer
///
/// Conceptually we keep a count array `c` where `c[v] = 1` if sequence number
/// `v` is currently missing, else `0`. A rank query is then a *prefix sum* of
/// `c`, and insert/remove are *point updates*. A flat array makes one of those
/// `O(1)` and the other `O(W)`; a Fenwick tree makes **both `O(log W)`**.
///
/// The trick is that each tree slot stores a partial sum covering a range whose
/// length is the slot index's lowest set bit, `lowbit(i) = i & (-i)`. Slot `i`
/// covers the `lowbit(i)` cells ending at `i`, i.e. `(i - lowbit(i), i]`:
///
/// ```text
/// cells:    1    2    3    4    5    6    7    8
/// tree[1] = █                                      {1}
/// tree[2] = █────█                                 {1,2}
/// tree[3] =           █                            {3}
/// tree[4] = █────█────█────█                       {1,2,3,4}
/// tree[6] =                     █────█             {5,6}
/// tree[8] = █────█────█────█────█────█────█────█   {1..8}
/// ```
///
/// * [`prefix`] sums `c[1..=i]` by walking *down*, stripping the lowest set bit
///   each step (`i -= lowbit(i)`) so the disjoint covered ranges tile `1..=i`.
///   e.g. `prefix(7)` adds `tree[7]{7} + tree[6]{5,6} + tree[4]{1..4}`.
/// * [`point_add`] fixes every slot that covers cell `i` by walking *up*
///   (`i += lowbit(i)`).
///
/// Both walks flip one bit per step, so they touch at most `log2(W)` slots.
///
/// # Sliding window
///
/// Sequence numbers climb without bound (billions per run), but only a small
/// window is ever missing at once. So tree index `1` represents `base` (not 0),
/// the tree holds `cap` slots, and it can address numbers in `[base, base+cap)`.
/// When an insert would overflow the right edge, [`ensure_can_hold`] rebuilds
/// the tree, rebasing `base` up to the smallest still-stored number (found via
/// the downward [`min_value`] descent) to reclaim the empty low slots, and grows
/// `cap` only if that is still not enough. Memory therefore stays proportional
/// to the reordering depth, not the total packet count.
///
/// [`prefix`]: MissingRankTree::prefix
/// [`point_add`]: MissingRankTree::point_add
/// [`ensure_can_hold`]: MissingRankTree::ensure_can_hold
/// [`min_value`]: MissingRankTree::min_value
#[derive(Default)]
struct MissingRankTree {
    /// 1-based Fenwick tree; `tree[0]` is unused. Slots hold 0/1 counts but use
    /// a signed type so removals can subtract.
    tree: Vec<i64>,
    /// Sequence number represented by tree index 1.
    base: u64,
    /// Number of stored sequence numbers.
    len: u64,
}

impl MissingRankTree {
    fn cap(&self) -> usize {
        self.tree.len().saturating_sub(1)
    }

    fn len(&self) -> u64 {
        self.len
    }

    /// Add `delta` to cell `i`, updating every slot that covers it by walking
    /// up the tree (`i += lowbit(i)`), where `lowbit(i)` is
    /// `i.isolate_lowest_one()`.
    fn point_add(&mut self, mut i: usize, delta: i64) {
        let cap = self.cap();
        while i <= cap {
            self.tree[i] += delta;
            i += i.isolate_lowest_one();
        }
    }

    /// Sum of `tree[1..=i]`, walking down the tree by stripping the lowest set
    /// bit each step (`i -= lowbit(i)`) so the covered ranges tile `1..=i`.
    fn prefix(&self, mut i: usize) -> i64 {
        let mut sum = 0;
        while i > 0 {
            sum += self.tree[i];
            i -= i.isolate_lowest_one();
        }
        sum
    }

    /// Count of stored sequence numbers strictly less than `seq`.
    fn rank_below(&self, seq: u64) -> u64 {
        if seq <= self.base {
            return 0;
        }
        // Value `seq` lives at index `seq - base + 1`; everything strictly below
        // it is the prefix up to index `seq - base`.
        self.prefix((seq - self.base) as usize) as u64
    }

    /// Insert every number in `range` (assumed to be entirely above all stored
    /// numbers, which holds for the gap opened by a new high-water arrival).
    fn insert_range(&mut self, range: std::ops::Range<u64>) {
        if range.is_empty() {
            return;
        }
        // Make sure the window can index the whole range; `range.start` is a
        // valid new base because every stored number is below it.
        self.ensure_can_hold(range.end - 1, range.start);
        for seq in range.clone() {
            let i = (seq - self.base) as usize + 1;
            self.point_add(i, 1);
        }
        self.len += range.end - range.start;
    }

    /// Remove a single stored number (it must currently be present).
    fn remove(&mut self, seq: u64) {
        let i = (seq - self.base) as usize + 1;
        self.point_add(i, -1);
        self.len -= 1;
    }

    /// Guarantee that `max_seq` is addressable, rebasing onto the smallest
    /// stored number (or `empty_base` when nothing is stored) and growing the
    /// tree only if that is still not enough.
    ///
    /// Rebasing to the smallest stored number is the highest safe new `base`:
    /// every slot below it is empty and can be dropped, while nothing at or
    /// above it may be discarded. This is what lets the window slide forward
    /// instead of the array growing toward the total packet count.
    fn ensure_can_hold(&mut self, max_seq: u64, empty_base: u64) {
        if (self.base + self.cap() as u64) > max_seq {
            return;
        }
        let new_base = if self.len == 0 {
            empty_base
        } else {
            self.min_value()
        };
        let needed = (max_seq - new_base + 1) as usize;
        let mut new_cap = self.cap().max(1);
        while new_cap < needed {
            new_cap *= 2;
        }

        let values = self.drain_values();
        self.tree = vec![0; new_cap + 1];
        self.base = new_base;
        self.len = values.len() as u64;
        for seq in values {
            let i = (seq - self.base) as usize + 1;
            self.point_add(i, 1);
        }
    }

    /// Smallest stored sequence number (caller guarantees `len > 0`).
    ///
    /// This is the `k`-th-element query with `k = 1`: the smallest index whose
    /// prefix sum reaches 1. Instead of summing prefixes one index at a time, we
    /// descend the tree from the largest power-of-two stride down to 1, taking a
    /// step only while the running count stays below the target. Each slot we
    /// consider already holds the sum of a whole block, so this is `O(log W)`
    /// rather than the `O(W)` of scanning for the first non-empty cell.
    fn min_value(&self) -> u64 {
        let cap = self.cap();
        let mut pos = 0usize;
        let mut remaining = 1i64;
        let mut step = 1usize << (usize::BITS - 1 - cap.leading_zeros());
        while step > 0 {
            let next = pos + step;
            if next <= cap && self.tree[next] < remaining {
                pos = next;
                remaining -= self.tree[next];
            }
            step >>= 1;
        }
        self.base + pos as u64 // index `pos + 1` -> value `base + pos`
    }

    /// Reconstruct the stored sequence numbers, consuming the tree's contents.
    fn drain_values(&mut self) -> Vec<u64> {
        let cap = self.cap();
        // Invert the Fenwick construction in place so `tree[i]` holds the raw
        // per-index count again.
        for i in (1..=cap).rev() {
            let j = i + i.isolate_lowest_one();
            if j <= cap {
                self.tree[j] -= self.tree[i];
            }
        }
        let mut values = Vec::with_capacity(self.len as usize);
        for i in 1..=cap {
            if self.tree[i] != 0 {
                values.push(self.base + (i - 1) as u64);
            }
        }
        values
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReorderingMetric {
    OutOfOrderCounter,
    KendallTauDistance,
}

/// Tracks passage times between delay thresholds.
/// Passage time is the time it takes for the delay to travel from one extreme
/// (mean - std) to the other (mean + std) or vice versa.
pub struct PassageTimeTracker {
    lower_threshold_ns: f64,
    upper_threshold_ns: f64,
    /// None = waiting for first threshold hit, Some(true) = last hit was upper, Some(false) = last hit was lower
    last_hit_was_upper: Option<bool>,
    /// Simulation time when we first hit the last threshold
    last_hit_time_ns: f64,
}

impl PassageTimeTracker {
    pub fn new(mean_ns: f64, std_ns: f64) -> Self {
        Self {
            lower_threshold_ns: (mean_ns - std_ns).max(0.),
            upper_threshold_ns: mean_ns + std_ns,
            last_hit_was_upper: None,
            last_hit_time_ns: 0.,
        }
    }

    /// Check if a delay crosses a threshold and return the passage time if applicable.
    /// Returns Some(pt_ns) if we crossed from one threshold to the other.
    pub fn check_delay(&mut self, delay_ns: f64, current_simulation_time_ns: f64) -> Option<f64> {
        let hit_lower = delay_ns <= self.lower_threshold_ns;
        let hit_upper = delay_ns >= self.upper_threshold_ns;

        // Check if we hit any threshold
        if !hit_lower && !hit_upper {
            return None;
        }

        let current_hit_is_upper = hit_upper;

        match self.last_hit_was_upper {
            None => {
                // First threshold hit - just record it
                self.last_hit_was_upper = Some(current_hit_is_upper);
                self.last_hit_time_ns = current_simulation_time_ns;
                None
            }
            Some(last_was_upper) if last_was_upper != current_hit_is_upper => {
                // We crossed from one threshold to the other - this is a passage!
                let pt_ns = current_simulation_time_ns - self.last_hit_time_ns;
                self.last_hit_was_upper = Some(current_hit_is_upper);
                self.last_hit_time_ns = current_simulation_time_ns;
                Some(pt_ns)
            }
            _ => {
                // We hit the same threshold again - no PT
                // Do not update the last_hit_time_ns because we stil want to track the time when we first hit this threshold
                None
            }
        }
    }
}

pub struct SimulationStatistics {
    inter_arrival_time_ns: Histogram<u64>,
    delay_ns: Histogram<u64>,
    reordering_metric: ReorderingMetric,
    /// Sequence numbers we have expected by now but not yet seen.
    /// Sized to the current reordering depth, not the total packet count.
    kendall_tau_missing: MissingRankTree,
    /// Next sequence number we expect (one past the highest one seen so far).
    kendall_tau_next_expected: u64,
    kendall_tau_distance: u64,
    tracked_packets: usize,
    out_of_order_packets: usize,
    expected_sequence_number: Option<u64>,
    measured_passage_time_ns: Histogram<u64>,
    measured_pt_tracker: Option<PassageTimeTracker>,
    theoretical_passage_time_ns: Histogram<u64>,
    theoretical_pt_tracker: Option<PassageTimeTracker>,
    dropped_packets_loss: u64,
    dropped_packets_limiter: u64,
    dropped_packets_buffer_full: u64,
    burst_spacing_offset_ns: Histogram<u64>,
    dropped_packets_burst_virtual_queue: u64,
}

impl SimulationStatistics {
    pub fn new(reordering_metric: ReorderingMetric) -> Self {
        Self {
            inter_arrival_time_ns: Histogram::<u64>::new(3).unwrap(),
            delay_ns: Histogram::<u64>::new(3).unwrap(),
            reordering_metric,
            kendall_tau_missing: MissingRankTree::default(),
            kendall_tau_next_expected: 0,
            kendall_tau_distance: 0,
            tracked_packets: 0,
            out_of_order_packets: 0,
            expected_sequence_number: None,
            measured_passage_time_ns: Histogram::<u64>::new(3).unwrap(),
            measured_pt_tracker: None,
            theoretical_passage_time_ns: Histogram::<u64>::new(3).unwrap(),
            theoretical_pt_tracker: None,
            dropped_packets_loss: 0,
            dropped_packets_limiter: 0,
            dropped_packets_buffer_full: 0,
            burst_spacing_offset_ns: Histogram::<u64>::new(3).unwrap(),
            dropped_packets_burst_virtual_queue: 0,
        }
    }

    pub fn init_passage_time_tracker(&mut self, mean_ns: f64, std_ns: f64) {
        self.measured_pt_tracker = Some(PassageTimeTracker::new(mean_ns, std_ns));
        self.theoretical_pt_tracker = Some(PassageTimeTracker::new(mean_ns, std_ns));
    }

    pub fn record_inter_arrival_time(&mut self, inter_arrival_time_ns: f64) {
        self.inter_arrival_time_ns
            .record(inter_arrival_time_ns.round() as u64)
            .ok();
    }

    pub fn record_delay(&mut self, delay_ns: f64, current_simulation_time_ns: f64) {
        self.delay_ns.record(delay_ns.round() as u64).ok();

        // Check for measured passage time if tracker is initialized
        if let Some(ref mut tracker) = self.measured_pt_tracker
            && let Some(pt_ns) = tracker.check_delay(delay_ns, current_simulation_time_ns)
        {
            self.measured_passage_time_ns
                .record(pt_ns.round() as u64)
                .ok();
        }
    }

    pub fn record_theoretical_delay(&mut self, delay_ns: f64, current_simulation_time_ns: f64) {
        // Check for theoretical passage time if tracker is initialized
        if let Some(ref mut tracker) = self.theoretical_pt_tracker
            && let Some(pt_ns) = tracker.check_delay(delay_ns, current_simulation_time_ns)
        {
            self.theoretical_passage_time_ns
                .record(pt_ns.round() as u64)
                .ok();
        }
    }

    pub fn record_dropped_packet_loss(&mut self) {
        self.dropped_packets_loss += 1;
    }

    pub fn record_dropped_packet_limiter(&mut self) {
        self.dropped_packets_limiter += 1;
    }

    pub fn record_dropped_packet_buffer_full(&mut self) {
        self.dropped_packets_buffer_full += 1;
    }

    pub fn record_burst_spacing_offset(&mut self, offset_ns: f64) {
        self.burst_spacing_offset_ns
            .record(offset_ns.round() as u64)
            .ok();
    }

    pub fn record_dropped_packet_burst_virtual_queue(&mut self) {
        self.dropped_packets_burst_virtual_queue += 1;
    }

    pub fn record_sequence_number(&mut self, sequence_number: u64) {
        self.tracked_packets += 1;
        match self.reordering_metric {
            ReorderingMetric::KendallTauDistance => {
                self.record_kendall_tau(sequence_number);
            }
            ReorderingMetric::OutOfOrderCounter => match self.expected_sequence_number {
                None => {
                    self.expected_sequence_number = Some(sequence_number + 1);
                }
                Some(expected) if sequence_number < expected => {
                    self.out_of_order_packets += 1;
                }
                Some(_) => {
                    self.expected_sequence_number = Some(sequence_number + 1);
                }
            },
        }
    }

    /// Online inversion counting via a "missing packets" structure.
    /// `kendall_tau_missing` holds sequence numbers that we have already expected
    /// (i.e. below the current high-water mark `kendall_tau_next_expected`) but
    /// not yet seen. At each arrival `a` we count the inversions for which `a`
    /// is the *earlier, larger* element of the pair: those are exactly the
    /// already-missing numbers `m < a`.
    ///
    /// The structure is an order-statistics (Fenwick) tree, so both the
    /// per-arrival rank query and the gap inserts are `O(log W)` in the
    /// reordering window `W`, versus the `O(W)` range scan a plain `BTreeSet`
    /// would need for the rank query.
    fn record_kendall_tau(&mut self, sequence_number: u64) {
        if sequence_number >= self.kendall_tau_next_expected {
            // New high-water mark: every skipped number in between becomes a
            // gap. All currently-tracked missing numbers are strictly less than
            // `sequence_number`, so the new inversions equal the whole set size.
            self.kendall_tau_missing
                .insert_range(self.kendall_tau_next_expected..sequence_number);
            self.kendall_tau_next_expected = sequence_number + 1;
            self.kendall_tau_distance += self.kendall_tau_missing.len();
        } else {
            // Late arrival fills a gap; only the missing numbers below it form
            // inversions paired with this packet.
            self.kendall_tau_distance += self.kendall_tau_missing.rank_below(sequence_number);
            self.kendall_tau_missing.remove(sequence_number);
        }
    }

    pub fn get_results(&mut self) -> SimulationResults {
        let n = self.tracked_packets;

        let (
            kendall_tau_distance,
            kendall_tau_distance_div_n,
            out_of_order_packets,
            out_of_order_packets_div_n,
        ) = match self.reordering_metric {
            ReorderingMetric::KendallTauDistance => {
                let ktd = self.kendall_tau_distance as usize;
                let ktd_div_n = if n > 0 { ktd as f64 / n as f64 } else { 0.0 };
                (ktd, ktd_div_n, 0, 0.0)
            }
            ReorderingMetric::OutOfOrderCounter => {
                let ooo_div_n = if n > 0 {
                    self.out_of_order_packets as f64 / n as f64
                } else {
                    0.0
                };
                (0, 0.0, self.out_of_order_packets, ooo_div_n)
            }
        };

        SimulationResults {
            tracked_packets: n,
            inter_arrival_time_mean_ns: self.inter_arrival_time_ns.mean(),
            inter_arrival_time_std_ns: self.inter_arrival_time_ns.stdev(),
            delay_mean_ns: self.delay_ns.mean(),
            delay_std_ns: self.delay_ns.stdev(),
            kendall_tau_distance,
            kendall_tau_distance_div_n,
            out_of_order_packets,
            out_of_order_packets_div_n,
            tracked_theoretical_passage_times: self.theoretical_passage_time_ns.len() as usize,
            theoretical_passage_time_mean_ns: self.theoretical_passage_time_ns.mean(),
            theoretical_passage_time_std_ns: self.theoretical_passage_time_ns.stdev(),
            tracked_measured_passage_times: self.measured_passage_time_ns.len() as usize,
            measured_passage_time_mean_ns: self.measured_passage_time_ns.mean(),
            measured_passage_time_std_ns: self.measured_passage_time_ns.stdev(),
            dropped_packets_loss: self.dropped_packets_loss,
            dropped_packets_limiter: self.dropped_packets_limiter,
            dropped_packets_buffer_full: self.dropped_packets_buffer_full,
            burst_spacing_offset_mean_ns: self.burst_spacing_offset_ns.mean(),
            burst_spacing_offset_std_ns: self.burst_spacing_offset_ns.stdev(),
            burst_spacing_offset_max_ns: self.burst_spacing_offset_ns.max(),
            dropped_packets_burst_virtual_queue: self.dropped_packets_burst_virtual_queue,
        }
    }
}

impl Default for SimulationStatistics {
    fn default() -> Self {
        Self::new(ReorderingMetric::OutOfOrderCounter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(vec![0,1,2,3,4], 0)]
    #[test_case(vec![1,0,2,3,4], 1)]
    #[test_case(vec![1,2,0,3,4], 2)]
    #[test_case(vec![2,1,0,3,4], 3)]
    #[test_case(vec![2,1,0,4,3], 4)]
    #[test_case(vec![2,1,4,0,3], 5)]
    #[test_case(vec![2,4,1,0,3], 6)]
    #[test_case(vec![2,4,1,3,0], 7)]
    #[test_case(vec![4,2,1,3,0], 8)]
    #[test_case(vec![4,2,3,1,0], 9)]
    #[test_case(vec![4,3,2,1,0], 10)]
    fn kendall_tau_distance_tests(sequence_numbers: Vec<u64>, actual_distance: usize) {
        let mut simulation_statistics =
            SimulationStatistics::new(ReorderingMetric::KendallTauDistance);

        for seq in &sequence_numbers {
            simulation_statistics.record_sequence_number(*seq);
        }

        assert_eq!(
            simulation_statistics.kendall_tau_distance as usize,
            actual_distance
        )
    }

    #[test_case(vec![0,1,2,3,4], 0)]
    #[test_case(vec![1,0,2,3,4], 1)]
    #[test_case(vec![0,1,2,4,3], 1)]
    #[test_case(vec![1,0,3,2,4], 2)]
    #[test_case(vec![4,3,2,1,0], 4)]
    fn out_of_order_counter_tests(sequence_numbers: Vec<u64>, expected_out_of_order: usize) {
        let mut simulation_statistics =
            SimulationStatistics::new(ReorderingMetric::OutOfOrderCounter);

        for seq in &sequence_numbers {
            simulation_statistics.record_sequence_number(*seq);
        }

        assert_eq!(
            simulation_statistics.out_of_order_packets,
            expected_out_of_order
        );
        assert_eq!(
            simulation_statistics.tracked_packets,
            sequence_numbers.len()
        );
    }

    /// Brute-force inversion count: number of pairs that arrive out of order.
    fn brute_force_inversions(sequence_numbers: &[u64]) -> u64 {
        let mut inversions = 0;
        for i in 0..sequence_numbers.len() {
            for j in (i + 1)..sequence_numbers.len() {
                if sequence_numbers[i] > sequence_numbers[j] {
                    inversions += 1;
                }
            }
        }
        inversions
    }

    /// Cheap deterministic PRNG so the test needs no extra dependency.
    fn next_rand(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn kendall_tau_matches_brute_force_under_random_reordering() {
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..200 {
            // Build a permutation of 0..n, then locally shuffle it so the
            // reordering window (and the Fenwick rebasing/growth) is exercised.
            let n = 1 + (next_rand(&mut state) % 400) as usize;
            let mut seq: Vec<u64> = (0..n as u64).collect();
            for i in 0..n {
                let span = (next_rand(&mut state) % 64) as usize;
                let j = (i + (next_rand(&mut state) as usize % (span + 1))).min(n - 1);
                seq.swap(i, j);
            }

            let mut stats = SimulationStatistics::new(ReorderingMetric::KendallTauDistance);
            for &s in &seq {
                stats.record_sequence_number(s);
            }

            assert_eq!(
                stats.kendall_tau_distance,
                brute_force_inversions(&seq),
                "mismatch for sequence {seq:?}"
            );
        }
    }

    #[test]
    fn kendall_tau_handles_large_gaps_and_rebasing() {
        // A genuine permutation of 0..N (every number still arrives) but with a
        // few far-displaced elements, forcing the window to grow and the Fenwick
        // tree to rebase onto its smallest stored element while a deep gap is
        // open. Numbers 1..N-2 arrive in order, bracketed by the two extremes
        // arriving in reverse, which keeps a wide window populated throughout.
        let n = 2000u64;
        let mut seq: Vec<u64> = vec![n - 1];
        seq.extend(1..(n - 1));
        seq.push(0);
        let mut stats = SimulationStatistics::new(ReorderingMetric::KendallTauDistance);
        for &s in &seq {
            stats.record_sequence_number(s);
        }
        assert_eq!(stats.kendall_tau_distance, brute_force_inversions(&seq));
    }
}
