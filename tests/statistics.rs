//! Characterisation tests of the statistics code that stays in the redesign.
//!
//! Only the statistics that feed a result column are covered: delay mean/std,
//! the reordering metric (`out_of_order_packets`, `kendall_tau_distance_div_n`)
//! the theoretical passage-time mean and the limiter drop counter. `record_delay` takes just the delay
//! (the measured passage-time tracker, which needed the time, is gone).

use lib::statistics::{PassageTimeTracker, ReorderingMetric, SimulationStatistics};

fn record_seqs(metric: ReorderingMetric, seqs: &[u64]) -> SimulationStatistics {
    let mut stats = SimulationStatistics::new(metric);
    for &s in seqs {
        stats.record_sequence_number(s);
    }
    stats
}

// ---------------------------------------------------------------- reordering

#[test]
fn reordering_metric_serde_names_are_snake_case() {
    assert_eq!(
        serde_json::to_string(&ReorderingMetric::OutOfOrderCounter).unwrap(),
        "\"out_of_order_counter\""
    );
    assert_eq!(
        serde_json::to_string(&ReorderingMetric::KendallTauDistance).unwrap(),
        "\"kendall_tau_distance\""
    );
    let parsed: ReorderingMetric = serde_json::from_str("\"kendall_tau_distance\"").unwrap();
    assert_eq!(parsed, ReorderingMetric::KendallTauDistance);
}

#[test]
fn default_statistics_use_out_of_order_counter() {
    let mut stats = SimulationStatistics::default();
    for s in [1, 0, 2] {
        stats.record_sequence_number(s);
    }
    let r = stats.get_results();
    assert_eq!(r.out_of_order_packets, 1);
    assert_eq!(r.kendall_tau_distance_div_n, 0.0);
}

#[test]
fn out_of_order_counter_results() {
    // 1 0 3 2 4: packets 0 and 2 arrive after a larger sequence number.
    let mut stats = record_seqs(ReorderingMetric::OutOfOrderCounter, &[1, 0, 3, 2, 4]);
    let r = stats.get_results();
    assert_eq!(r.out_of_order_packets, 2);
    // The other metric's column is zero when it is not selected.
    assert_eq!(r.kendall_tau_distance_div_n, 0.0);
}

#[test]
fn out_of_order_counter_ignores_gaps_from_dropped_packets() {
    // Sequence numbers 1, 3 and 4 never arrive (dropped): no reordering.
    let mut stats = record_seqs(ReorderingMetric::OutOfOrderCounter, &[0, 2, 5, 6]);
    let r = stats.get_results();
    assert_eq!(r.out_of_order_packets, 0);
}

#[test]
fn kendall_tau_div_n_is_inversions_per_tracked_packet() {
    // 2 1 0 3: three inversions (2>1, 2>0, 1>0).
    let mut stats = record_seqs(ReorderingMetric::KendallTauDistance, &[2, 1, 0, 3]);
    let r = stats.get_results();
    assert!((r.kendall_tau_distance_div_n - 0.75).abs() < 1e-12);
    // The other metric's column is zero when it is not selected.
    assert_eq!(r.out_of_order_packets, 0);
}

#[test]
fn kendall_tau_counts_never_arriving_sequence_numbers_as_inversions() {
    // Characterises the semantics of the statistic itself (unchanged): a
    // sequence number that never arrives stays "missing" forever and forms an
    // inversion with every later arrival. The simulation therefore feeds this
    // function a dense ordinal of the ACCEPTED packets, so drops never create
    // gaps (see the `kendall_tau_with_*` tests in `model.rs`).
    // [0, 2, 4, 5] has no reordering among the arrived packets, yet:
    // at 2 missing {1} -> +1, at 4 missing {1,3} -> +2, at 5 -> +2 = 5
    // inversions over 4 tracked packets.
    let mut stats = record_seqs(ReorderingMetric::KendallTauDistance, &[0, 2, 4, 5]);
    assert!((stats.get_results().kendall_tau_distance_div_n - 5. / 4.).abs() < 1e-12);
    // [0, 4, 2, 5]: at 4 missing {1,2,3} -> +3, at 2 (below it: {1}) -> +1,
    // at 5 missing {1,3} -> +2 = 6 inversions over 4 tracked packets.
    let mut stats = record_seqs(ReorderingMetric::KendallTauDistance, &[0, 4, 2, 5]);
    assert!((stats.get_results().kendall_tau_distance_div_n - 6. / 4.).abs() < 1e-12);
}

#[test]
fn empty_statistics_have_zero_counts_and_no_division_by_zero() {
    for metric in [
        ReorderingMetric::OutOfOrderCounter,
        ReorderingMetric::KendallTauDistance,
    ] {
        let mut stats = SimulationStatistics::new(metric);
        let r = stats.get_results();
        assert_eq!(r.out_of_order_packets, 0);
        assert_eq!(r.kendall_tau_distance_div_n, 0.0);
        assert!(r.delay_mean_ns.is_finite());
        assert!(r.theoretical_passage_time_mean_ns.is_finite());
    }
}

// ------------------------------------------------------- mean / std of delays

#[test]
fn delay_mean_and_std_are_population_statistics() {
    let mut stats = SimulationStatistics::default();
    for d in [100., 200., 300.] {
        stats.record_delay(d);
    }
    let r = stats.get_results();
    assert!((r.delay_mean_ns - 200.).abs() < 1e-9);
    // population std: sqrt((100^2 + 0 + 100^2) / 3)
    assert!((r.delay_std_ns - (20000f64 / 3.).sqrt()).abs() < 1e-6);
}

#[test]
fn delays_are_rounded_to_whole_ns_before_recording() {
    let mut stats = SimulationStatistics::default();
    stats.record_delay(100.4); // -> 100
    stats.record_delay(100.6); // -> 101
    let r = stats.get_results();
    assert!((r.delay_mean_ns - 100.5).abs() < 1e-9);
}

// ------------------------------------------------------- passage-time tracker

#[test]
fn passage_time_tracker_measures_time_between_opposite_thresholds() {
    // lower threshold 8, upper threshold 12
    let mut t = PassageTimeTracker::new(10., 2.);
    assert_eq!(t.check_delay(10., 50.), None); // between thresholds
    assert_eq!(t.check_delay(8., 100.), None); // first hit: lower, only recorded
    assert_eq!(t.check_delay(7., 150.), None); // same threshold again
    assert_eq!(t.check_delay(12., 300.), Some(200.)); // upper: 300 - 100 (first lower hit)
    assert_eq!(t.check_delay(13., 400.), None); // same threshold again
    assert_eq!(t.check_delay(9., 420.), None); // between again
    assert_eq!(t.check_delay(8., 450.), Some(150.)); // lower: 450 - 300
}

#[test]
fn passage_time_tracker_thresholds_are_inclusive() {
    let mut t = PassageTimeTracker::new(10., 2.);
    assert_eq!(t.check_delay(7.999, 0.), None); // lower hit (first)
    assert_eq!(t.check_delay(11.999, 10.), None); // not yet upper
    assert_eq!(t.check_delay(12., 20.), Some(20.)); // exactly the upper threshold counts
    assert_eq!(t.check_delay(8., 35.), Some(15.)); // exactly the lower threshold counts
}

#[test]
fn passage_time_tracker_clamps_lower_threshold_at_zero() {
    // mean - std = -4 -> lower threshold 0
    let mut t = PassageTimeTracker::new(1., 5.);
    assert_eq!(t.check_delay(0.5, 0.), None); // above the clamped lower threshold
    assert_eq!(t.check_delay(0., 10.), None); // first hit: lower
    assert_eq!(t.check_delay(6., 25.), Some(15.)); // upper = 6
}

#[test]
fn statistics_track_the_theoretical_passage_time() {
    let mut stats = SimulationStatistics::default();
    stats.init_passage_time_tracker(10., 2.);
    // lower at t=0, upper at t=1000 -> one passage of 1000 ns
    stats.record_theoretical_delay(8., 0.);
    stats.record_theoretical_delay(12., 1000.);
    // `record_delay` feeds the delay statistic only, never the passage time
    stats.record_delay(8.);
    stats.record_delay(12.);
    let r = stats.get_results();
    assert!((r.theoretical_passage_time_mean_ns - 1000.).abs() < 1.5);
}

#[test]
fn limiter_drops_are_counted() {
    let mut stats = SimulationStatistics::default();
    assert_eq!(stats.get_results().dropped_packets_limiter, 0);
    stats.record_dropped_packet_limiter();
    stats.record_dropped_packet_limiter();
    assert_eq!(stats.get_results().dropped_packets_limiter, 2);
}

#[test]
fn passage_times_are_not_tracked_without_an_initialised_tracker() {
    let mut stats = SimulationStatistics::default();
    stats.record_theoretical_delay(8., 0.);
    stats.record_theoretical_delay(12., 1000.);
    let r = stats.get_results();
    assert_eq!(r.theoretical_passage_time_mean_ns, 0.0);
}
