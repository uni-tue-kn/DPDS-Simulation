//! One simulation run: a stream of packet arrivals feeding the real
//! `dpds_core::buffer::Buffer`, drained at the buffer's send times.
//!
//! Two event sources are merged:
//!
//! * the **arrival queue**: always exactly one pending arrival, so it is just
//!   the index of the next packet (arrival `k` happens at `k * iat`);
//! * the **buffer**: its head's send time (`front_time`) is the next transmit
//!   event.
//!
//! # Rules
//!
//! * Tie rule: a transmit event at time `t` is processed before an arrival at
//!   `t`, so a buffer slot freed at `t` is usable by the arrival at `t`.
//! * Arrivals with `t <= duration` are processed (inclusive); afterwards the
//!   buffer is drained, so every accepted packet departs.
//! * A packet is popped at `max(clock, send time)`: exactly at its send time,
//!   or, in FIFO mode when blocked behind a later one, right after its
//!   predecessor.
//! * The packet pointer handed to the buffer is the opaque value `slot + 1`
//!   and never dereferenced. `slot` indexes a side table with the arrival index
//!   and the accepted-packet ordinal of the packet; the slot is released when
//!   the packet departs (or was not accepted), so the table is as large as the
//!   most packets ever in flight. The arrival time is recomputed from the
//!   arrival index.

use std::ffi::c_void;

use dpds_core::{
    Config as BufferConfig, DelayerConfig, LimiterConfig,
    buffer::{Buffer, PushOutcome},
    delayer::RVDistribution,
    dropper::LossModel,
    limiter::{Limiter, LimiterMode},
};

use crate::config::{Config, SimulationConfig};
use crate::results::SimulationResults;
use crate::statistics::{ReorderingMetric, SimulationStatistics};

/// Buffer capacity in packets when the config does not set `buffer_capacity`.
pub const DEFAULT_BUFFER_CAPACITY: usize = 1 << 21;

/// One transmitted packet, reported by [`run_simulation_traced`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Departure {
    /// Arrival index (counts every arrival, so dropped packets leave gaps;
    /// the reordering statistics get the dense ordinal among accepted packets).
    pub seq: u64,
    /// Arrival time (the `now` passed to `push_outcome`).
    pub rx_time_ns: f64,
    /// Send time the buffer promised (`Accepted { send_time_ns }`).
    pub send_time_ns: u64,
    /// Simulation time of the transmit event.
    pub tx_time_ns: f64,
}

pub fn run_simulation(config: &Config, run: u32) -> SimulationResults {
    run_simulation_traced(config, run, |_| {})
}

/// Like [`run_simulation`], additionally calling `on_departure` once per
/// transmitted packet, in departure order.
pub fn run_simulation_traced<F: FnMut(&Departure)>(
    config: &Config,
    run: u32,
    mut on_departure: F,
) -> SimulationResults {
    let sim = &config.simulation_config;
    let mut buffer = Buffer::try_new(buffer_config(config, run))
        .unwrap_or_else(|e| panic!("invalid simulation config: {e}"));
    let mut statistics = SimulationStatistics::new(
        sim.reordering_metric
            .unwrap_or(ReorderingMetric::OutOfOrderCounter),
    );
    if let Some(delay) = &config.delay_config {
        statistics.init_passage_time_tracker(delay.mean_ns, delay.std_ns);
    }

    let frame_byte = sim.average_frame_size_byte.ceil() as u32;
    let inter_arrival_time_ns = inter_arrival_time_ns(sim);
    let duration_ns = duration_ns(sim);

    // Virtual queue at X% utilization: the burst spacing a link running at
    // rate / X would need for the departure stream.
    let mut burst_virtual_queue = Limiter::new(
        LimiterMode::Spacer,
        true,
        sim.rate_gbps * 1e9 / 0.90,
        f64::MAX,
    );

    let mut next_seq: u64 = 0;
    // Number of accepted packets so far: the dense ordinal of the next one.
    let mut accepted: u64 = 0;
    // Side table of the packets in flight: slot -> (arrival index, accepted
    // ordinal), with the free slots.
    let mut slots: Vec<(u64, u64)> = Vec::new();
    let mut free_slots: Vec<usize> = Vec::new();
    let mut clock_ns = 0.;
    let mut last_departure_ns: Option<f64> = None;

    loop {
        let next_arrival_ns = next_seq as f64 * inter_arrival_time_ns;
        let arrival_pending = next_arrival_ns <= duration_ns;

        match buffer.front_time() {
            // Transmit event (wins ties with an arrival).
            Some(front_ns) if !arrival_pending || front_ns <= next_arrival_ns => {
                // Never go back in time: a head blocked behind a later packet
                // has an older send time than the clock.
                clock_ns = f64::max(clock_ns, front_ns);
                let (ptr, send_time_ns) = buffer
                    .pop_due_with_time(clock_ns)
                    .expect("the head is due at max(clock, front_time)");
                let slot = ptr as usize - 1;
                let (seq, ordinal) = slots[slot];
                free_slots.push(slot);
                let rx_time_ns = seq as f64 * inter_arrival_time_ns;

                if let Some(last_ns) = last_departure_ns {
                    statistics.record_inter_arrival_time(clock_ns - last_ns);
                }
                last_departure_ns = Some(clock_ns);
                statistics.record_delay(clock_ns - rx_time_ns, clock_ns);
                // Dropped packets must not count as reordering: feed the
                // ordinal among the accepted packets, not the arrival index.
                statistics.record_sequence_number(ordinal);
                match burst_virtual_queue.limiter_calculate_offset_ns(clock_ns, frame_byte) {
                    Some(offset_ns) => statistics.record_burst_spacing_offset(offset_ns),
                    None => statistics.record_dropped_packet_burst_virtual_queue(),
                }
                on_departure(&Departure {
                    seq,
                    rx_time_ns,
                    send_time_ns,
                    tx_time_ns: clock_ns,
                });
            }
            // Arrival event.
            _ if arrival_pending => {
                clock_ns = next_arrival_ns;
                // Tentative slot with the next ordinal, kept only if accepted.
                let slot = free_slots.pop().unwrap_or_else(|| {
                    slots.push((0, 0));
                    slots.len() - 1
                });
                slots[slot] = (next_seq, accepted);
                let ptr = (slot + 1) as *mut c_void;
                let outcome = buffer.push_outcome(ptr, frame_byte, clock_ns);
                if matches!(outcome, PushOutcome::Accepted { .. }) {
                    accepted += 1;
                } else {
                    free_slots.push(slot);
                }
                match outcome {
                    PushOutcome::Accepted { send_time_ns } => statistics
                        .record_theoretical_delay(send_time_ns as f64 - clock_ns, clock_ns),
                    PushOutcome::Lost => statistics.record_dropped_packet_loss(),
                    PushOutcome::LimiterDrop => statistics.record_dropped_packet_limiter(),
                    PushOutcome::BufferFull => statistics.record_dropped_packet_buffer_full(),
                }
                next_seq += 1;
            }
            // No more arrivals and the buffer is empty.
            _ => break,
        }
    }

    statistics.get_results()
}

fn inter_arrival_time_ns(sim: &SimulationConfig) -> f64 {
    let rate_pps = sim.rate_gbps * 1e9 / ((sim.average_frame_size_byte + 20.) * 8.);
    1e9 / rate_pps
}

fn duration_ns(sim: &SimulationConfig) -> f64 {
    (sim.duration_s * 1e9).round()
}

/// Number of packets one run offers to the buffer: arrivals at `k * iat`
/// for all `k` with `k * iat <= duration`.
pub fn arrivals_per_run(sim: &SimulationConfig) -> u64 {
    (duration_ns(sim) / inter_arrival_time_ns(sim)).floor() as u64 + 1
}

/// The `dpds-core` configuration for run `run` of `config`.
fn buffer_config(config: &Config, run: u32) -> BufferConfig {
    let sim = &config.simulation_config;
    let delayer = match &config.delay_config {
        Some(delay) => DelayerConfig {
            distribution: delay.rv_distribution,
            mean_ns: delay.mean_ns,
            std_ns: delay.std_ns,
            activate_correlation: delay.activate_correlation,
            half_life_period_ns: delay.half_life_period_ns,
        },
        None => DelayerConfig {
            distribution: RVDistribution::DeterministicDistribution,
            mean_ns: 0.,
            std_ns: 0.,
            activate_correlation: false,
            half_life_period_ns: 0.,
        },
    };
    let limiter = match &config.limiter_config {
        Some(limiter) => LimiterConfig {
            mode: limiter.mode,
            activate: true,
            rate_bps: limiter.rate_bps,
            limit_bit: limiter.limit_bit,
        },
        None => LimiterConfig {
            mode: LimiterMode::Spacer,
            activate: false,
            rate_bps: 0.,
            limit_bit: 0.,
        },
    };
    BufferConfig {
        buffer_capacity: sim.buffer_capacity.unwrap_or(DEFAULT_BUFFER_CAPACITY),
        reordering: sim.activate_reordering,
        delayer,
        limiter,
        loss_model: sim.loss_model.unwrap_or(LossModel::Random(0.0)),
        seed: sim.seed.map(|seed| seed.wrapping_add(u64::from(run))),
    }
}
