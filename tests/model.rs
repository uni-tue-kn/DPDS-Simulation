//! Model tests of the redesigned simulation (spec v9): arrival queue +
//! the real `dpds_core::buffer::Buffer`.
//!
//! # Test-facing API fixed by these tests
//!
//! ```text
//! lib::simulation::run_simulation(config: &Config, run: u32) -> SimulationResults
//! lib::simulation::run_simulation_traced<F: FnMut(&Departure)>(
//!         config: &Config, run: u32, on_departure: F) -> SimulationResults
//!     // `run_simulation` = traced with a no-op closure. The closure is called
//!     // once per transmitted packet, in departure order.
//! #[derive(Debug, Clone, Copy, PartialEq)]
//! lib::simulation::Departure {
//!     seq: u64,          // arrival index 0,1,2,..; counts EVERY arrival, so
//!                        // dropped packets leave gaps
//!     rx_time_ns: f64,   // `now` passed to push_outcome
//!     send_time_ns: u64, // `Accepted { send_time_ns }` of push_outcome
//!     tx_time_ns: f64,   // simulation time of the transmit event (pop time)
//! }
//! lib::simulation::DEFAULT_BUFFER_CAPACITY: usize = 1 << 21
//! lib::results::CSV_HEADER: [&str; 23] or &[&str]   // the 23 column names, see csv_header_is_exact
//! lib::results::SimulationResults
//!     - dropped_packets_bandwidth REMOVED
//!     + dropped_packets_loss: u64, dropped_packets_limiter: u64,
//!       dropped_packets_buffer_full: u64      (all other fields unchanged)
//! lib::config::SimulationConfig (all `#[serde(default, skip_serializing_if = "Option::is_none")]`)
//!     + buffer_capacity: Option<usize>   // default (None): 2^21 (`DEFAULT_BUFFER_CAPACITY`)
//!     + loss_model: Option<dpds_core::dropper::LossModel>  // None = Random(0.0)
//!     + seed: Option<u64>                // Some(s): run r is seeded with s + r
//! ```
//! `write_to_csv(&self, &Arc<Mutex<Writer<File>>>, run: u32)` is unchanged.
//!
//! # Model rules the tests pin (decisions the spec left open, flagged)
//!
//! * Arrival k happens at `k * iat` with `iat = 1e9 / (rate_gbps*1e9 /
//!   ((frame+20)*8))`; arrivals with `t <= duration_ns` (inclusive, as before)
//!   are processed; afterwards the buffer is drained, so every accepted packet
//!   departs.
//! * Frame length passed to `push_outcome` = `average_frame_size_byte.ceil()`.
//!   dpds-core adds 4 (FEC) and 20 (L1) bytes: limiter size =
//!   `(ceil(frame) + 4 + 20) * 8` bit.
//! * Pop at exact send times: `tx_time = max(clock, send_time)`; in FIFO mode a
//!   packet blocked behind a later one leaves right after it (same instant).
//! * TIE RULE at equal times: a transmit event at time t is processed BEFORE an
//!   arrival at time t (so a buffer slot freed at t is usable by an arrival at
//!   t). Pinned by `equal_time_transmit_is_processed_before_arrival_*`.
//! * EVERY departure is tracked (no "skip the first packet" as in the old
//!   code): `tracked_packets` = number of departures; the inter-departure
//!   histogram has n-1 samples; the burst virtual queue sees every departure.
//! * Theoretical passage time: tracker fed at arrival of every ACCEPTED packet
//!   with `send_time_ns - rx_time` (u64 truncation included) at time rx.
//!   Measured: fed at every departure with `tx - rx` at time tx.
//! * Burst virtual queue (as before): `Limiter::new(Spacer, true,
//!   rate_gbps*1e9/0.90, f64::MAX)` called at the departure time with
//!   `frame.ceil()` bytes, i.e. WITHOUT the +4 bytes.
//!
//! All hand-derived numbers below follow dpds-core `buffer.rs`/`limiter.rs`
//! (b825fc6) and were cross-checked with an independent Python re-derivation.

mod common;

use common::{read_csv, run_sweep};
use dpds_core::limiter::{Limiter, LimiterMode};
use lib::config::Config;
use lib::results::{CSV_HEADER, SimulationResults};
use lib::simulation::{Departure, run_simulation, run_simulation_traced};
use lib::statistics::PassageTimeTracker;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::File;
use std::sync::{Arc, Mutex};

// ------------------------------------------------------------------ fixtures

/// Offered load: rate 1.2 Gbit/s, frame 1480 B -> 12000 bit on the wire ->
/// iat = exactly 10000 ns (all quantities below are exact in binary floats).
const IAT: f64 = 10_000.0;

#[derive(Clone)]
struct Scenario {
    sim: Value,
    delay: Option<Value>,
    limiter: Option<Value>,
}

/// 0.001 s -> arrivals at 0, 10000, ..., 1_000_000 ns = 101 packets.
fn scenario() -> Scenario {
    Scenario {
        sim: json!({
            "duration_s": 0.001, "runs": 1, "rate_gbps": 1.2,
            "average_frame_size_byte": 1480, "activate_reordering": false,
            "csv_output_path": "unused.csv", "seed": 42
        }),
        delay: None,
        limiter: None,
    }
}

impl Scenario {
    fn with(mut self, key: &str, value: Value) -> Self {
        self.sim[key] = value;
        self
    }
    fn reorder(self, on: bool) -> Self {
        self.with("activate_reordering", json!(on))
    }
    fn delay(mut self, distribution: &str, mean_ns: f64, std_ns: f64) -> Self {
        self.delay = Some(json!({
            "mean_ns": mean_ns, "std_ns": std_ns, "rv_distribution": distribution,
            "activate_correlation": false, "half_life_period_ns": 0
        }));
        self
    }
    /// The limiter has no limit of its own: dpds-core derives it from the
    /// buffer capacity (`derived_limit_bit`, capacity x 12336 bit).
    fn limiter(mut self, mode: &str, rate_bps: f64) -> Self {
        self.limiter = Some(json!({"mode": mode, "rate_bps": rate_bps}));
        self
    }
    fn capacity(self, packets: usize) -> Self {
        self.with("buffer_capacity", json!(packets))
    }
    fn build(&self) -> Config {
        let mut v = json!({ "simulation_config": self.sim.clone() });
        if let Some(d) = &self.delay {
            v["delay_config"] = d.clone();
        }
        if let Some(l) = &self.limiter {
            v["limiter_config"] = l.clone();
        }
        serde_json::from_value(v).expect("scenario config must parse")
    }
}

fn trace(config: &Config, run: u32) -> (SimulationResults, Vec<Departure>) {
    let mut departures = Vec::new();
    let results = run_simulation_traced(config, run, |d| departures.push(*d));
    (results, departures)
}

fn iat_ns(config: &Config) -> f64 {
    let s = &config.simulation_config;
    let rate_pps = s.rate_gbps * 1e9 / ((s.average_frame_size_byte + 20.) * 8.);
    1e9 / rate_pps
}

fn expected_arrivals(config: &Config) -> u64 {
    let duration_ns = (config.simulation_config.duration_s * 1e9).round();
    (duration_ns / iat_ns(config)).floor() as u64 + 1
}

fn total_drops(r: &SimulationResults) -> u64 {
    r.dropped_packets_loss + r.dropped_packets_limiter + r.dropped_packets_buffer_full
}

/// Tolerance for values read back from the 3-significant-digit histograms.
fn hist_close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= 0.0015 * expected.abs() + 1.0
}

fn sorted_by_seq(departures: &[Departure]) -> Vec<Departure> {
    let mut v = departures.to_vec();
    v.sort_by_key(|d| d.seq);
    v
}

/// Oracle: aggregated results recomputed from the departure trace.
fn check_aggregates(config: &Config, r: &SimulationResults, deps: &[Departure]) {
    let s = &config.simulation_config;
    let n = deps.len();
    assert!(n > 10, "scenario too small to be meaningful");
    assert_eq!(r.tracked_packets, n);

    // delay = tx - rx
    let delay_mean = deps
        .iter()
        .map(|d| d.tx_time_ns - d.rx_time_ns)
        .sum::<f64>()
        / n as f64;
    assert!(
        hist_close(r.delay_mean_ns, delay_mean),
        "delay mean {} vs oracle {delay_mean}",
        r.delay_mean_ns
    );

    // inter-departure mean telescopes to (last - first) / (n - 1)
    let span = deps[n - 1].tx_time_ns - deps[0].tx_time_ns;
    let inter = span / (n - 1) as f64;
    assert!(
        hist_close(r.inter_arrival_time_mean_ns, inter),
        "inter-departure mean {} vs oracle {inter}",
        r.inter_arrival_time_mean_ns
    );

    // passage times
    let delay_cfg = config.delay_config.as_ref().expect("delay config");
    let mut theoretical = PassageTimeTracker::new(delay_cfg.mean_ns, delay_cfg.std_ns);
    let theoretical_pts: Vec<f64> = sorted_by_seq(deps)
        .iter()
        .filter_map(|d| theoretical.check_delay(d.send_time_ns as f64 - d.rx_time_ns, d.rx_time_ns))
        .collect();
    assert!(!theoretical_pts.is_empty(), "scenario has no passages");
    assert_eq!(
        r.tracked_theoretical_passage_times,
        theoretical_pts.len(),
        "theoretical passage count"
    );
    let mean = theoretical_pts.iter().sum::<f64>() / theoretical_pts.len() as f64;
    assert!(hist_close(r.theoretical_passage_time_mean_ns, mean));

    let mut measured = PassageTimeTracker::new(delay_cfg.mean_ns, delay_cfg.std_ns);
    let measured_pts: Vec<f64> = deps
        .iter()
        .filter_map(|d| measured.check_delay(d.tx_time_ns - d.rx_time_ns, d.tx_time_ns))
        .collect();
    assert_eq!(
        r.tracked_measured_passage_times,
        measured_pts.len(),
        "measured passage count"
    );
    if !measured_pts.is_empty() {
        let mean = measured_pts.iter().sum::<f64>() / measured_pts.len() as f64;
        assert!(hist_close(r.measured_passage_time_mean_ns, mean));
    }

    // burst virtual queue: independent Spacer fed with the departure times
    let mut queue = Limiter::new(
        LimiterMode::Spacer,
        true,
        s.rate_gbps * 1e9 / 0.90,
        f64::MAX,
    );
    let frame = s.average_frame_size_byte.ceil() as u32;
    let offsets: Vec<f64> = deps
        .iter()
        .map(|d| {
            queue
                .limiter_calculate_offset_ns(d.tx_time_ns, frame)
                .expect("limit f64::MAX never drops")
        })
        .collect();
    let max = offsets.iter().cloned().fold(0., f64::max);
    let mean = offsets.iter().map(|o| o.round()).sum::<f64>() / n as f64;
    assert_eq!(r.dropped_packets_burst_virtual_queue, 0);
    assert!(
        r.burst_spacing_offset_max_ns as f64 >= max.round()
            && r.burst_spacing_offset_max_ns as f64 <= max * 1.0015 + 2.,
        "burst max {} vs oracle {max}",
        r.burst_spacing_offset_max_ns
    );
    assert!(
        hist_close(r.burst_spacing_offset_mean_ns, mean),
        "burst mean {} vs oracle {mean}",
        r.burst_spacing_offset_mean_ns
    );
}

// -------------------------------------------------------------------- config

#[test]
fn new_optional_config_fields_default_to_none() {
    let c = scenario().build();
    let mut value = serde_json::to_value(&c).unwrap();
    assert!(c.simulation_config.buffer_capacity.is_none());
    assert!(c.simulation_config.loss_model.is_none());
    assert_eq!(c.simulation_config.seed, Some(42)); // set by the fixture
    let sim = value["simulation_config"].as_object_mut().unwrap();
    sim.remove("seed");
    let c: Config = serde_json::from_value(value).unwrap();
    assert!(c.simulation_config.seed.is_none());
}

#[test]
fn new_optional_config_fields_parse() {
    let c = scenario()
        .with("buffer_capacity", json!(5))
        .with("loss_model", json!({"Random": 0.25}))
        .with("seed", json!(9))
        .build();
    assert_eq!(c.simulation_config.buffer_capacity, Some(5));
    assert_eq!(c.simulation_config.seed, Some(9));
    assert!(matches!(
        c.simulation_config.loss_model,
        Some(dpds_core::dropper::LossModel::Random(p)) if p == 0.25
    ));

    let c = scenario()
        .with(
            "loss_model",
            json!({"GilbertElliot": [0.1, 0.2, 0.9, 0.3, true]}),
        )
        .build();
    assert!(matches!(
        c.simulation_config.loss_model,
        Some(dpds_core::dropper::LossModel::GilbertElliot(p, r, k, h, true))
            if p == 0.1 && r == 0.2 && k == 0.9 && h == 0.3
    ));
}

#[test]
fn new_optional_config_fields_survive_a_serde_round_trip() {
    let c = scenario()
        .with("buffer_capacity", json!(7))
        .with("loss_model", json!({"Random": 0.5}))
        .build();
    let back: Config = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
    assert_eq!(back.simulation_config.buffer_capacity, Some(7));
    assert_eq!(back.simulation_config.seed, Some(42));
    assert!(matches!(
        back.simulation_config.loss_model,
        Some(dpds_core::dropper::LossModel::Random(p)) if p == 0.5
    ));
}

#[test]
fn simulation_configs_json_still_parses_without_the_new_fields() {
    let text = include_str!("../simulation_configs.json");
    let configs: Vec<Config> = serde_json::from_str(text).unwrap();
    assert!(configs.iter().all(|c| {
        c.simulation_config.buffer_capacity.is_none()
            && c.simulation_config.loss_model.is_none()
            && c.simulation_config.seed.is_none()
    }));
}

// ----------------------------------------------------------------------- csv

const EXPECTED_HEADER: [&str; 23] = [
    "run",
    "tracked_packets",
    "inter_arrival_time_mean_ns",
    "inter_arrival_time_std_ns",
    "delay_mean_ns",
    "delay_std_ns",
    "kendall_tau_distance",
    "kendall_tau_distance_div_n",
    "out_of_order_packets",
    "out_of_order_packets_div_n",
    "tracked_theoretical_passage_times",
    "theoretical_passage_time_mean_ns",
    "theoretical_passage_time_std_ns",
    "tracked_measured_passage_times",
    "measured_passage_time_mean_ns",
    "measured_passage_time_std_ns",
    // `dropped_packets_bandwidth` is replaced in place by the three below
    "dropped_packets_loss",
    "dropped_packets_limiter",
    "dropped_packets_buffer_full",
    "burst_spacing_offset_mean_ns",
    "burst_spacing_offset_std_ns",
    "burst_spacing_offset_max_ns",
    "dropped_packets_burst_virtual_queue",
];

#[test]
fn csv_header_is_exact() {
    let header: Vec<&str> = CSV_HEADER.to_vec();
    assert_eq!(header, EXPECTED_HEADER);
}

fn temp_csv(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("dpds-simulation-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{name}-{}.csv", std::process::id()))
}

/// Writes header + one row through the public CSV path and reads it back.
fn csv_row_of(name: &str, results: &SimulationResults, run: u32) -> (Vec<String>, Vec<String>) {
    let path = temp_csv(name);
    let writer = Arc::new(Mutex::new(csv::Writer::from_writer(
        File::create(&path).unwrap(),
    )));
    writer
        .lock()
        .unwrap()
        .write_record(CSV_HEADER.iter().copied())
        .unwrap();
    results.write_to_csv(&writer, run).unwrap();
    writer.lock().unwrap().flush().unwrap();
    let (header, mut rows) = read_csv(&path);
    assert_eq!(rows.len(), 1);
    (header, rows.remove(0))
}

#[test]
fn csv_row_has_one_field_per_header_column_in_header_order() {
    let cfg = scenario()
        .with("loss_model", json!({"Random": 0.3}))
        .delay("NormalDistribution", 100_000., 30_000.)
        .capacity(10)
        .limiter("Spacer", 0.5e9)
        .build();
    let r = run_simulation(&cfg, 0);
    // lost packets still consume spacing slots, so the derived limit
    // (10 x 12336 bit) is reached although the buffer never needs to hold it
    assert!(r.dropped_packets_loss > 0 && r.dropped_packets_limiter > 0);
    let (header, row) = csv_row_of("row-shape", &r, 3);
    assert_eq!(row.len(), header.len());
    let field =
        |name: &str| -> &String { &row[header.iter().position(|h| h == name).expect(name)] };
    assert_eq!(field("run"), "3");
    assert_eq!(field("tracked_packets"), &r.tracked_packets.to_string());
    assert_eq!(
        field("dropped_packets_loss"),
        &r.dropped_packets_loss.to_string()
    );
    assert_eq!(
        field("dropped_packets_limiter"),
        &r.dropped_packets_limiter.to_string()
    );
    assert_eq!(
        field("dropped_packets_buffer_full"),
        &r.dropped_packets_buffer_full.to_string()
    );
    assert_eq!(
        field("burst_spacing_offset_max_ns"),
        &r.burst_spacing_offset_max_ns.to_string()
    );
    assert_eq!(
        field("delay_mean_ns").parse::<f64>().unwrap(),
        r.delay_mean_ns
    );
}

#[test]
fn binary_rows_equal_library_runs_with_seed_plus_run_index() {
    let configs = r#"[{"simulation_config": {"duration_s": 0.0004, "runs": 3, "rate_gbps": 1.2,
        "average_frame_size_byte": 1480, "activate_reordering": true,
        "csv_output_path": "e2e.csv", "seed": 5, "loss_model": {"Random": 0.1}},
      "delay_config": {"mean_ns": 100000, "std_ns": 30000,
        "rv_distribution": "NormalDistribution", "activate_correlation": false,
        "half_life_period_ns": 0}}]"#;
    let parsed: Vec<Config> = serde_json::from_str(configs).unwrap();
    let results_dir = run_sweep("model-e2e", configs, Some("2"));
    let (_, rows) = read_csv(&results_dir.join("e2e.csv"));
    assert_eq!(rows.len(), 3);
    for run in 0..3u32 {
        // looked up by run index: row ORDER is checked in sweep_driver.rs
        let row = rows
            .iter()
            .find(|row| row[0] == run.to_string())
            .unwrap_or_else(|| panic!("no row for run {run}"));
        let r = run_simulation(&parsed[0], run);
        let (_, expected) = csv_row_of(&format!("e2e-{run}"), &r, run);
        assert_eq!(row, &expected, "run {run}");
    }
}

// ------------------------------------------------- smoke over the real sweep

#[test]
fn smoke_run_of_a_sample_of_simulation_configs_conserves_packets() {
    let text = include_str!("../simulation_configs.json");
    let configs: Vec<Config> = serde_json::from_str(text).unwrap();
    let mut seen = HashSet::new();
    for mut c in configs {
        let key = format!(
            "{:?}|{:?}|{:?}|{}|{:?}|{}",
            c.delay_config.as_ref().map(|d| d.rv_distribution),
            c.delay_config.as_ref().map(|d| d.activate_correlation),
            c.limiter_config.as_ref().map(|l| l.mode),
            c.simulation_config.activate_reordering,
            c.simulation_config.reordering_metric,
            c.simulation_config.rate_gbps,
        );
        if !seen.insert(key.clone()) {
            continue;
        }
        c.simulation_config.duration_s = 0.0005;
        c.simulation_config.runs = 1;
        let r = run_simulation(&c, 0);
        let arrivals = expected_arrivals(&c);
        let accounted = r.tracked_packets as u64 + total_drops(&r);
        assert!(
            accounted.abs_diff(arrivals) <= 1,
            "{key}: {accounted} accounted vs {arrivals} arrivals"
        );
        assert!(r.tracked_packets > 0 || total_drops(&r) > 0, "{key}");
    }
    assert!(seen.len() >= 9, "sample too small: {}", seen.len());
}

// --------------------------------------------- deterministic delay, FIFO/heap

fn constant_delay_departs_at_exact_send_times(reordering: bool) {
    // 1_000_000.5 ns: dpds-core truncates the send time to whole ns.
    let cfg = scenario()
        .delay("DeterministicDistribution", 1_000_000.5, 0.)
        .reorder(reordering)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len(), 101, "arrivals at 0..=1_000_000 ns, inclusive");
    for (k, d) in deps.iter().enumerate() {
        assert_eq!(d.seq, k as u64);
        assert_eq!(d.rx_time_ns, k as f64 * IAT);
        assert_eq!(d.send_time_ns, 1_000_000 + 10_000 * k as u64, "truncated");
        assert_eq!(d.tx_time_ns, d.send_time_ns as f64, "pop at the send time");
    }
    assert_eq!(r.tracked_packets, 101);
    assert!(hist_close(r.delay_mean_ns, 1_000_000.));
    assert!(r.delay_std_ns < 1.0, "every delay identical");
    assert!(hist_close(r.inter_arrival_time_mean_ns, IAT));
    assert!(
        r.inter_arrival_time_std_ns < 1.0,
        "inter-departure = inter-arrival"
    );
    assert_eq!(r.out_of_order_packets, 0);
    assert_eq!(total_drops(&r), 0);
}

#[test]
fn fifo_with_constant_delay_delays_every_packet_by_exactly_the_mean() {
    constant_delay_departs_at_exact_send_times(false);
}

#[test]
fn reordering_with_constant_delay_does_not_reorder() {
    constant_delay_departs_at_exact_send_times(true);
}

#[test]
fn constant_delay_with_sub_nanosecond_arrivals_keeps_sequence_order() {
    // 1000 Gbit/s, 1 B frames: iat = 0.168 ns, many arrivals per whole ns of
    // send time. Heap keys have 1/64 ns resolution (0.168 ns = ~10.75 units),
    // so arrivals at different instants never tie and the order is preserved.
    let cfg = scenario()
        .with("rate_gbps", json!(1000.0))
        .with("average_frame_size_byte", json!(1.0))
        .with("duration_s", json!(2e-7))
        .delay("DeterministicDistribution", 1000.0, 0.)
        .reorder(true)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len() as u64, expected_arrivals(&cfg));
    assert!(deps.len() > 1000);
    for (k, d) in deps.iter().enumerate() {
        assert_eq!(d.seq, k as u64);
        assert_eq!(d.tx_time_ns, d.send_time_ns as f64);
        if k > 0 {
            assert!(d.send_time_ns >= deps[k - 1].send_time_ns);
        }
    }
    assert_eq!(r.out_of_order_packets, 0);
    assert!(deps[0].send_time_ns >= 1000);
}

#[test]
fn default_buffer_capacity_does_not_bind() {
    // ~1000 packets in flight (100 ms delay at 10 us spacing).
    let cfg = scenario()
        .with("duration_s", json!(0.01))
        .delay("DeterministicDistribution", 100_000_000., 0.)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len() as u64, expected_arrivals(&cfg));
    assert_eq!(total_drops(&r), 0);
}

#[test]
fn default_buffer_capacity_is_two_to_the_twenty_one() {
    // M2: each run reserves 16 B x capacity up front; 2^21 = 32 MiB, which
    // leaves a margin over the ~0.96 M packets in flight of the largest
    // committed config (mean + 6 std)
    assert_eq!(lib::simulation::DEFAULT_BUFFER_CAPACITY, 1 << 21);
    assert_eq!(lib::simulation::DEFAULT_BUFFER_CAPACITY, 2_097_152);
}

#[test]
fn default_capacity_binds_exactly_at_two_to_the_twenty_one_packets_in_flight() {
    // 100 Gbit/s, 64 B frames -> iat = 6.72 ns; every packet is held for 1 s
    // (far beyond the 14.5 ms of arrivals), so nothing departs while arrivals
    // last and the buffer holds exactly `capacity` packets, FIFO and heap.
    for reordering in [false, true] {
        let cfg = scenario()
            .with("duration_s", json!(0.0145)) // 2.16 M arrivals
            .with("rate_gbps", json!(100.0))
            .with("average_frame_size_byte", json!(64))
            .delay("DeterministicDistribution", 1_000_000_000., 0.)
            .reorder(reordering)
            .build();
        let arrivals = expected_arrivals(&cfg);
        assert!(arrivals > (1 << 21) + 50_000, "{arrivals} arrivals");
        let r = run_simulation(&cfg, 0);
        assert_eq!(r.tracked_packets as u64, 1 << 21, "reordering={reordering}");
        assert_eq!(
            r.dropped_packets_buffer_full,
            arrivals - (1 << 21),
            "reordering={reordering}"
        );
        assert_eq!(r.dropped_packets_loss + r.dropped_packets_limiter, 0);
    }
}

#[test]
fn an_explicit_capacity_overrides_the_default() {
    let cfg = scenario()
        .with("buffer_capacity", json!(2))
        .delay("DeterministicDistribution", 1_000_000_000., 0.)
        .build();
    let r = run_simulation(&cfg, 0);
    assert_eq!(r.tracked_packets, 2);
    assert_eq!(r.dropped_packets_buffer_full, expected_arrivals(&cfg) - 2);
}

// ----------------------------------------------------- random delay: ordering

#[test]
fn fifo_with_random_delay_departs_in_sequence_order_with_head_of_line_blocking() {
    let cfg = scenario()
        .with("duration_s", json!(0.005)) // 501 packets
        .delay("NormalDistribution", 1_000_000., 300_000.)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len(), 501);
    let mut previous_tx = f64::NEG_INFINITY;
    let mut blocked = 0;
    for (k, d) in deps.iter().enumerate() {
        assert_eq!(d.seq, k as u64, "FIFO: departures in sequence order");
        // measured delay >= theoretical (sampled) delay
        assert!(d.tx_time_ns >= d.send_time_ns as f64);
        assert!(d.tx_time_ns - d.rx_time_ns >= d.send_time_ns as f64 - d.rx_time_ns);
        // a packet leaves at its send time or, if blocked, right after its predecessor
        assert_eq!(
            d.tx_time_ns,
            previous_tx.max(d.send_time_ns as f64),
            "seq {k}"
        );
        if d.tx_time_ns > d.send_time_ns as f64 {
            blocked += 1;
        }
        previous_tx = d.tx_time_ns;
    }
    assert!(
        blocked > 0,
        "std 300 us >> iat 10 us must block heads of line"
    );
    assert_eq!(r.out_of_order_packets, 0);
    assert_eq!(total_drops(&r), 0);
    check_aggregates(&cfg, &r, &deps);
}

#[test]
fn reordering_with_random_delay_departs_in_send_time_order() {
    let cfg = scenario()
        .with("duration_s", json!(0.005))
        .delay("NormalDistribution", 1_000_000., 300_000.)
        .reorder(true)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len(), 501);
    for (i, d) in deps.iter().enumerate() {
        assert_eq!(
            d.tx_time_ns, d.send_time_ns as f64,
            "popped exactly when due"
        );
        if i > 0 {
            assert!(
                d.send_time_ns >= deps[i - 1].send_time_ns,
                "sorted by send time"
            );
        }
    }
    let seqs: HashSet<u64> = deps.iter().map(|d| d.seq).collect();
    assert_eq!(
        seqs,
        (0..501).collect::<HashSet<u64>>(),
        "every packet once"
    );

    // out-of-order counter recomputed from the trace (same rule as statistics.rs)
    let mut expected_next = 0u64;
    let mut out_of_order = 0usize;
    for d in &deps {
        if d.seq < expected_next {
            out_of_order += 1;
        } else {
            expected_next = d.seq + 1;
        }
    }
    assert!(out_of_order > 0, "std 300 us >> iat 10 us must reorder");
    assert_eq!(r.out_of_order_packets, out_of_order);
    assert!((r.out_of_order_packets_div_n - out_of_order as f64 / 501.).abs() < 1e-12);
    check_aggregates(&cfg, &r, &deps);
}

#[test]
fn reordering_results_can_use_the_kendall_tau_metric() {
    let cfg = scenario()
        .with("duration_s", json!(0.002)) // 201 packets
        .with("reordering_metric", json!("kendall_tau_distance"))
        .delay("NormalDistribution", 1_000_000., 300_000.)
        .reorder(true)
        .build();
    let (r, deps) = trace(&cfg, 0);
    let mut inversions = 0usize;
    for i in 0..deps.len() {
        for j in (i + 1)..deps.len() {
            if deps[i].seq > deps[j].seq {
                inversions += 1;
            }
        }
    }
    assert!(inversions > 0);
    assert_eq!(r.kendall_tau_distance, inversions);
    assert_eq!(r.out_of_order_packets, 0);
}

// ------------------------------------------- Kendall tau with dropped packets

/// Oracle: inversions among the packets that actually departed, from the
/// departure trace (pairs transmitted in the opposite order of their arrival).
fn survivor_inversions(deps: &[Departure]) -> usize {
    let mut inversions = 0;
    for i in 0..deps.len() {
        for j in (i + 1)..deps.len() {
            if deps[i].seq > deps[j].seq {
                inversions += 1;
            }
        }
    }
    inversions
}

fn kendall(scenario: Scenario) -> Scenario {
    scenario.with("reordering_metric", json!("kendall_tau_distance"))
}

/// H1: lost / limited / buffer-full packets must never count as inversions.
fn assert_kendall_counts_only_survivors(cfg: &Config, expect_reordering: bool) {
    let (r, deps) = trace(cfg, 0);
    let arrivals = expected_arrivals(cfg);
    assert!(total_drops(&r) > 0, "scenario must drop packets");
    assert!(
        (deps.len() as u64) < arrivals
            && deps.iter().map(|d| d.seq).max().unwrap() >= deps.len() as u64,
        "arrival indices must keep their gaps (seq counts every arrival)"
    );
    let inversions = survivor_inversions(&deps);
    if expect_reordering {
        assert!(inversions > 0, "scenario must reorder survivors");
    } else {
        assert_eq!(inversions, 0);
    }
    assert_eq!(r.kendall_tau_distance, inversions);
    assert!(
        (r.kendall_tau_distance_div_n - inversions as f64 / deps.len() as f64).abs() < 1e-12,
        "div_n {} vs {inversions}/{}",
        r.kendall_tau_distance_div_n,
        deps.len()
    );
    assert_eq!(r.out_of_order_packets, 0, "the other metric stays unused");
}

#[test]
fn kendall_tau_with_a_loss_model_equals_the_inversions_of_the_survivors() {
    let cfg = kendall(
        scenario()
            .with("duration_s", json!(0.005)) // 501 arrivals
            .with("loss_model", json!({"Random": 0.3}))
            .delay("NormalDistribution", 1_000_000., 300_000.)
            .reorder(true),
    )
    .build();
    assert_kendall_counts_only_survivors(&cfg, true);
}

#[test]
fn kendall_tau_with_gilbert_elliot_burst_loss_equals_the_survivor_inversions() {
    let cfg = kendall(
        scenario()
            .with("duration_s", json!(0.005))
            .with(
                "loss_model",
                json!({"GilbertElliot": [0.2, 0.3, 0.9, 0.0, true]}),
            )
            .delay("NormalDistribution", 1_000_000., 300_000.)
            .reorder(true),
    )
    .build();
    assert_kendall_counts_only_survivors(&cfg, true);
}

#[test]
fn kendall_tau_with_a_tiny_capacity_equals_the_inversions_of_the_survivors() {
    // 10 slots, ~100 packets in flight (1 ms delay, 10 us iat): most arrivals
    // hit a full buffer.
    let cfg = kendall(
        scenario()
            .with("duration_s", json!(0.005))
            .with("buffer_capacity", json!(10))
            .delay("NormalDistribution", 1_000_000., 300_000.)
            .reorder(true),
    )
    .build();
    let (r, _) = trace(&cfg, 0);
    assert!(r.dropped_packets_buffer_full > 0);
    assert_kendall_counts_only_survivors(&cfg, true);
}

#[test]
fn kendall_tau_with_limiter_drops_is_zero_because_the_survivors_stay_in_order() {
    // Spacer at 0.5 Gbit/s under 1.2 Gbit/s offered load, with delay jitter.
    // Lost packets consume spacing slots, so the derived limit (100 x 12336
    // bit) is reached and the limiter drops as well.
    let cfg = kendall(
        scenario()
            .with("duration_s", json!(0.005))
            .with("loss_model", json!({"Random": 0.3}))
            .capacity(100)
            .delay("NormalDistribution", 300_000., 150_000.)
            .limiter("Spacer", 0.5e9)
            .reorder(true),
    )
    .build();
    let (r, _) = trace(&cfg, 0);
    assert!(r.dropped_packets_limiter > 0);
    // the Spacer serialises its (already delayed) packets: survivors stay in
    // order, so the distance must be 0 however many packets it drops
    assert_kendall_counts_only_survivors(&cfg, false);
}

#[test]
fn kendall_tau_with_all_three_drop_causes_equals_the_survivor_inversions_zero() {
    let cfg = kendall(
        scenario()
            .with("duration_s", json!(0.01))
            .with("loss_model", json!({"Random": 0.2}))
            .with("buffer_capacity", json!(40))
            .delay("NormalDistribution", 400_000., 200_000.)
            .limiter("Spacer", 0.9e9)
            .reorder(true),
    )
    .build();
    let (r, _) = trace(&cfg, 0);
    assert!(r.dropped_packets_loss > 0);
    assert!(r.dropped_packets_limiter + r.dropped_packets_buffer_full > 0);
    // (the Spacer keeps the survivors in order, see the limiter test above)
    assert_kendall_counts_only_survivors(&cfg, false);
}

#[test]
fn kendall_tau_without_reordering_is_zero_even_when_packets_are_dropped() {
    // FIFO keeps the order of the survivors: whatever is dropped, 0 inversions.
    for loss in [0.1, 0.5, 0.9] {
        let cfg = kendall(
            scenario()
                .with("duration_s", json!(0.005))
                .with("loss_model", json!({"Random": loss}))
                .delay("NormalDistribution", 1_000_000., 300_000.),
        )
        .build();
        let (r, deps) = trace(&cfg, 0);
        assert!(r.dropped_packets_loss > 0, "loss {loss}");
        assert!(
            deps.windows(2).all(|w| w[0].seq < w[1].seq),
            "FIFO order of the survivors"
        );
        assert_eq!(r.kendall_tau_distance, 0, "loss {loss}");
        assert_eq!(r.kendall_tau_distance_div_n, 0.0, "loss {loss}");
    }
}

#[test]
fn kendall_tau_without_survivors_is_zero() {
    let cfg = kendall(
        scenario()
            .with("loss_model", json!({"Random": 1.0}))
            .delay("NormalDistribution", 1_000_000., 300_000.)
            .reorder(true),
    )
    .build();
    let r = run_simulation(&cfg, 0);
    assert_eq!(r.tracked_packets, 0);
    assert_eq!(r.kendall_tau_distance, 0);
}

#[test]
fn kendall_tau_is_independent_of_which_packets_were_dropped() {
    // Same survivors' relative order => same distance, however sparse the
    // arrival indices are: compare the result with the oracle for several
    // seeds (different loss patterns).
    for seed in [1u64, 2, 3, 4, 5] {
        let cfg = kendall(
            scenario()
                .with("duration_s", json!(0.003))
                .with("seed", json!(seed))
                .with("loss_model", json!({"Random": 0.4}))
                .delay("NormalDistribution", 600_000., 250_000.)
                .reorder(true),
        )
        .build();
        let (r, deps) = trace(&cfg, 0);
        assert_eq!(
            r.kendall_tau_distance,
            survivor_inversions(&deps),
            "seed {seed}"
        );
    }
}

#[test]
fn out_of_order_counter_with_drops_still_matches_the_trace() {
    // The default metric ignores gaps already; the dense ordinal must not
    // change it.
    let cfg = scenario()
        .with("duration_s", json!(0.005))
        .with("loss_model", json!({"Random": 0.3}))
        .delay("NormalDistribution", 1_000_000., 300_000.)
        .reorder(true)
        .build();
    let (r, deps) = trace(&cfg, 0);
    let mut expected_next = 0u64;
    let mut out_of_order = 0usize;
    for d in &deps {
        if d.seq < expected_next {
            out_of_order += 1;
        } else {
            expected_next = d.seq + 1;
        }
    }
    assert!(out_of_order > 0);
    assert_eq!(r.out_of_order_packets, out_of_order);
    assert_eq!(r.kendall_tau_distance, 0);
}

// ------------------------------------------------------------------ limiters

/// Service time of one packet at 0.5 bit/ns.
const SERVICE_NS: u64 = 24_064;

/// `dpds_core::derived_limit_bit(capacity)` = capacity * 1542 * 8 = 12336 *
/// capacity bit (1518 B frame + 4 FEC + 20 L1, as the limiter counts a packet).
#[test]
fn derived_limit_is_capacity_times_the_maximum_wire_frame() {
    assert_eq!(dpds_core::MAX_FRAME_WIRE_BYTES, 1542);
    assert_eq!(dpds_core::derived_limit_bit(1), 12_336.0);
    assert_eq!(dpds_core::derived_limit_bit(40), 493_440.0);
    assert_eq!(
        dpds_core::derived_limit_bit(2_097_152),
        2_097_152.0 * 12_336.0
    );
}

/// Spacer at 0.5 bit/ns (0.5 Gbit/s), one packet = 12032 bit -> 24064 ns of
/// service, but one arrival per 10000 ns: a queue builds. With a delay of
/// 300 us and a capacity of 20 packets (limit 246720 bit, about 20.5 packets)
/// the buffer is the bottleneck: the queue the Spacer builds stays far below
/// its limit while the 300 us delay keeps the buffer full, so arrivals are
/// rejected as buffer-full. A buffer-full packet touches no limiter state,
/// so the accepted packets keep EXACT spacing: every departure is
/// 24064 ns after the previous one, the j-th is at 300000 + 24064 j (checked
/// with an independent re-implementation of buffer.rs/limiter.rs; before the
/// capacity check moved first, rejected packets advanced the Spacer and left
/// gaps of 48128 ns and more).
#[test]
fn buffer_full_packets_do_not_advance_the_spacer() {
    let cfg = scenario()
        .with("duration_s", json!(0.003))
        .capacity(20)
        .delay("DeterministicDistribution", 300_000., 0.)
        .limiter("Spacer", 0.5e9)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(expected_arrivals(&cfg), 301);
    assert_eq!(deps.len(), 133);
    assert_eq!(r.tracked_packets, 133);
    assert_eq!(r.dropped_packets_buffer_full, 168);
    assert_eq!(r.dropped_packets_limiter, 0);
    assert_eq!(r.dropped_packets_loss, 0);
    for (j, d) in deps.iter().enumerate() {
        assert_eq!(
            d.send_time_ns,
            300_000 + SERVICE_NS * j as u64,
            "departure {j} (arrival {})",
            d.seq
        );
        assert_eq!(d.tx_time_ns, d.send_time_ns as f64);
    }
    // the gaps in the arrival sequence prove that buffer-full drops happened
    // in between: the first 20 packets are accepted back to back, then the
    // buffer is full until the first one leaves at 300 us
    let first_21: Vec<u64> = deps.iter().take(21).map(|d| d.seq).collect();
    assert_eq!(
        first_21,
        (0..20).chain([30]).collect::<Vec<u64>>(),
        "first accepted arrivals"
    );
}

/// Delay 1 ms and capacity 13: arrivals 0..12 fill the buffer and are
/// spaced by the Spacer (send = 1e6 + 24064 k); 13..99 are rejected as
/// buffer-full. When packet 0 leaves at 1 ms, arrival 100 (t = 1 ms, tie rule:
/// transmit first) is accepted and finds an EMPTY Spacer: it leaves at exactly
/// its own time-to-send, 2 ms. Had the 87 rejected packets been accounted, it
/// would be queued behind their phantom backlog.
#[test]
fn first_packet_after_a_buffer_full_phase_finds_an_idle_spacer() {
    let cfg = scenario()
        .capacity(13)
        .delay("DeterministicDistribution", 1_000_000., 0.)
        .limiter("Spacer", 0.5e9)
        .build();
    let (r, deps) = trace(&cfg, 0);
    let first_14: Vec<(u64, u64)> = deps
        .iter()
        .take(14)
        .map(|d| (d.seq, d.send_time_ns))
        .collect();
    assert_eq!(
        first_14,
        vec![
            (0, 1_000_000),
            (1, 1_024_064),
            (2, 1_048_128),
            (3, 1_072_192),
            (4, 1_096_256),
            (5, 1_120_320),
            (6, 1_144_384),
            (7, 1_168_448),
            (8, 1_192_512),
            (9, 1_216_576),
            (10, 1_240_640),
            (11, 1_264_704),
            (12, 1_288_768),
            (100, 2_000_000),
        ]
    );
    assert_eq!(deps.len(), 14);
    assert_eq!(r.dropped_packets_buffer_full, 87);
    assert_eq!(r.dropped_packets_limiter, 0);
    assert_eq!(r.dropped_packets_loss, 0);
}

/// Lost packets are accounted by the limiter (they consume spacing slots),
/// buffer-full ones are not. Zero delay, Spacer at 0.5 bit/ns, capacity 2
/// (limit 24672 bit = 2.05 packets): without loss the queue is the only thing
/// that fills the Spacer, with 50 % loss the lost packets fill it as well and
/// the limiter starts to reject packets that the buffer alone would have
/// accepted.
#[test]
fn lost_packets_consume_spacing_slots_until_the_derived_limit_is_reached() {
    for seed in 1..=5u64 {
        let base = scenario().capacity(2).limiter("Spacer", 0.5e9);
        let with_loss = base
            .clone()
            .with("seed", json!(seed))
            .with("loss_model", json!({"Random": 0.5}))
            .build();
        let r = run_simulation(&with_loss, 0);
        assert!(r.dropped_packets_loss > 0, "seed {seed}");
        assert!(r.dropped_packets_limiter > 0, "seed {seed}");
        assert_eq!(
            total_drops(&r) + r.tracked_packets as u64,
            101,
            "seed {seed}"
        );
    }
}

/// Boundary of the derived limit of the SPACER. Rate 1e6 bit/s (drains
/// 10 bit per arrival, negligible), zero delay, capacity 40: limit =
/// 40 * 12336 = 493440 bit = 41.01 packets of 12032 bit, so the limiter lets
/// exactly the first 41 pushes pass (41 * 12032 = 493312 <= 493440 < 42 *
/// 12032) and rejects every later one. The Gilbert-Elliot model with
/// p = r = 1, k = 1, h = 0 loses every second packet, starting with the second
/// (the loss model is consulted for every pushed packet, also after the
/// limiter has rejected it), and a lost packet takes precedence over a limiter
/// rejection. Pushes 0..=40 pass the limiter: 21 survive (even indices).
/// Pushes 41..=100: the odd ones (30) are Lost, the even ones (30) are
/// LimiterDrop. A limit of 40 packets (12032 * 40, or 12304 * 40) would let
/// only 20 survive.
#[test]
fn spacer_limit_is_the_derived_capacity_times_12336_bit() {
    let cfg = scenario()
        .capacity(40)
        .with(
            "loss_model",
            json!({"GilbertElliot": [1.0, 1.0, 1.0, 0.0, true]}),
        )
        .limiter("Spacer", 1e6)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len(), 21);
    assert_eq!(r.dropped_packets_loss, 50);
    assert_eq!(r.dropped_packets_limiter, 30);
    assert_eq!(r.dropped_packets_buffer_full, 0);
    let seqs: Vec<u64> = deps.iter().map(|d| d.seq).collect();
    assert_eq!(seqs, (0..=40).step_by(2).collect::<Vec<u64>>());
}

#[test]
fn spacer_below_offered_load_without_limit_pressure_departs_every_24064_ns() {
    let cfg = scenario()
        .delay("DeterministicDistribution", 1_000_000., 0.)
        .limiter("Spacer", 0.5e9)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len(), 101);
    assert_eq!(total_drops(&r), 0);
    for (k, d) in deps.iter().enumerate() {
        assert_eq!(d.send_time_ns, 1_000_000 + 24_064 * k as u64);
        assert_eq!(d.tx_time_ns, d.send_time_ns as f64);
    }
    assert!(hist_close(r.inter_arrival_time_mean_ns, 24_064.));
}

#[test]
fn spacer_above_offered_load_neither_delays_nor_drops() {
    // 2 bit/ns: service 6016 ns < iat 10000 ns, the fill state is always empty.
    let cfg = scenario().limiter("Spacer", 2e9).build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len(), 101);
    assert_eq!(total_drops(&r), 0);
    for d in &deps {
        assert_eq!(d.tx_time_ns, d.rx_time_ns, "no delay config: zero delay");
    }
}

#[test]
fn frame_size_is_rounded_up_to_whole_bytes_for_the_limiter() {
    // 1479.2 B -> 1480 B -> 12032 bit -> 24064 ns spacing (floor/round would
    // give 24048). The backlog exists from the second packet on; allow 1 ns
    // for float noise because the arrival spacing is not an integer here.
    let cfg = scenario()
        .with("average_frame_size_byte", json!(1479.2))
        .limiter("Spacer", 0.5e9)
        .build();
    let (_, deps) = trace(&cfg, 0);
    assert!(deps.len() > 20);
    for pair in deps.windows(2).skip(5) {
        let gap = pair[1].tx_time_ns - pair[0].tx_time_ns;
        assert!((24_063.0..=24_065.0).contains(&gap), "gap {gap}");
    }
}

/// TBF at 0.5 bit/ns with capacity 3: the bucket is the derived 3 * 12336 =
/// 37008 bit (starts full), gains 5000 bit per arrival and a packet costs
/// 12032 bit. Zero delay, so the buffer never holds anything. Hand derivation
/// (checked with an independent re-implementation of limiter.rs): accept
/// 0,1,2,3 (37008 -> 24976, 17944, 10912, 3880 after), drop 4 (8880 < 12032),
/// accept 5, then 8, 10, 12, 15, 17, 20, 22, ... Out-of-budget packets are
/// dropped (not delayed) and accepted ones are not delayed either.
#[test]
fn token_bucket_filter_admits_bursts_then_drops_out_of_budget_packets() {
    let cfg = scenario()
        .capacity(3)
        .limiter("TokenBucketFilter", 0.5e9)
        .build();
    let (r, deps) = trace(&cfg, 0);
    let seqs: Vec<u64> = deps.iter().map(|d| d.seq).collect();
    assert_eq!(&seqs[..12], &[0, 1, 2, 3, 5, 8, 10, 12, 15, 17, 20, 22]);
    assert_eq!(deps.len(), 44);
    assert_eq!(r.dropped_packets_limiter, 57);
    assert_eq!(r.dropped_packets_loss, 0);
    assert_eq!(r.dropped_packets_buffer_full, 0);
    for d in &deps {
        assert_eq!(d.tx_time_ns, d.rx_time_ns, "TBF admits without delay");
    }
}

/// Boundary of the derived limit of the TOKEN BUCKET FILTER: the bucket
/// starts full with `capacity * 12336` bit and, at 1000 bit/s (0.01 bit
/// gained per arrival), is not refilled to speak of, so the number of
/// admitted packets is `floor(capacity * 12336 / 12032)`: 1 for capacity 1
/// and 2 for 2, but 39 for 39 (481104 bit: 39 * 12032 = 469248 leaves 11856 <
/// 12032) and 41 for 40 (493440 >= 41 * 12032 = 493312). A limit of
/// `capacity * 12032` bit would admit 40 for capacity 40.
#[test]
fn token_bucket_depth_is_the_derived_capacity_times_12336_bit() {
    for (capacity, admitted) in [(1usize, 1usize), (2, 2), (39, 39), (40, 41), (41, 42)] {
        let cfg = scenario()
            .capacity(capacity)
            .limiter("TokenBucketFilter", 1e3)
            .build();
        let (r, deps) = trace(&cfg, 0);
        assert_eq!(deps.len(), admitted, "capacity {capacity}");
        let seqs: Vec<u64> = deps.iter().map(|d| d.seq).collect();
        assert_eq!(seqs, (0..admitted as u64).collect::<Vec<_>>());
        assert_eq!(
            r.dropped_packets_limiter,
            101 - admitted as u64,
            "capacity {capacity}"
        );
        assert_eq!(r.dropped_packets_buffer_full + r.dropped_packets_loss, 0);
    }
}

/// Capacity 3, TBF at 0.5 bit/ns, delay 1 ms (100 inter-arrival times).
/// Arrivals 0,1,2 are accepted (bucket 37008 -> 10912 bit) and fill the
/// buffer; 3..99 are rejected as buffer-full and must NOT consume tokens: at
/// arrival 100 (tie rule: packet 0 leaves first) the bucket has refilled
/// completely, so 100, 101, 102 are accepted again, and so on. The cycle
/// repeats every 100 arrivals: accepted = k with k % 100 < 3. No arrival is
/// ever limiter-dropped. (With the old order, 3 consumed tokens and 4.. were
/// limiter drops.)
#[test]
fn buffer_full_packets_do_not_consume_token_bucket_tokens() {
    let cfg = scenario()
        .with("duration_s", json!(0.0035))
        .capacity(3)
        .delay("DeterministicDistribution", 1_000_000., 0.)
        .limiter("TokenBucketFilter", 0.5e9)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(expected_arrivals(&cfg), 351);
    let seqs: Vec<u64> = deps.iter().map(|d| d.seq).collect();
    assert_eq!(
        seqs,
        vec![0, 1, 2, 100, 101, 102, 200, 201, 202, 300, 301, 302]
    );
    assert_eq!(r.dropped_packets_buffer_full, 339);
    assert_eq!(r.dropped_packets_limiter, 0);
    assert_eq!(r.dropped_packets_loss, 0);
}

// ---------------------------------------------------------- capacity and ties

/// Delay exactly one inter-arrival time and capacity 1: packet k leaves at the
/// very instant packet k+1 arrives. TIE RULE: the transmit event goes first,
/// so the slot is free and nothing is dropped. (Arrival-first would drop every
/// second packet: 50 drops.)
fn equal_time_transmit_before_arrival(reordering: bool) {
    let cfg = scenario()
        .with("buffer_capacity", json!(1))
        .delay("DeterministicDistribution", IAT, 0.)
        .reorder(reordering)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(
        r.dropped_packets_buffer_full, 0,
        "transmit must precede arrival"
    );
    assert_eq!(deps.len(), 101);
    for (k, d) in deps.iter().enumerate() {
        assert_eq!(d.seq, k as u64);
        assert_eq!(d.send_time_ns, 10_000 * (k as u64 + 1));
        assert_eq!(d.tx_time_ns, d.send_time_ns as f64);
    }
}

#[test]
fn equal_time_transmit_is_processed_before_arrival_in_fifo_mode() {
    equal_time_transmit_before_arrival(false);
}

#[test]
fn equal_time_transmit_is_processed_before_arrival_in_reordering_mode() {
    equal_time_transmit_before_arrival(true);
}

/// Capacity 3, delay 100000.5 ns (-> 100000 ns, ten inter-arrival times): the
/// buffer fills with packets 0,1,2; at arrival 10 (t = 1e5) packet 0 leaves
/// first (tie rule) and 10 is accepted, 11 and 12 likewise, then 13..19 find the
/// buffer full, and so on: accepted = k with k % 10 < 3, i.e. 31 of 101.
fn capacity_causes_buffer_full_drops(reordering: bool) {
    let cfg = scenario()
        .with("buffer_capacity", json!(3))
        .delay("DeterministicDistribution", 100_000.5, 0.)
        .reorder(reordering)
        .build();
    let (r, deps) = trace(&cfg, 0);
    let expected: Vec<u64> = (0..=100).filter(|k| k % 10 < 3).collect();
    let seqs: Vec<u64> = deps.iter().map(|d| d.seq).collect();
    assert_eq!(seqs, expected);
    assert_eq!(deps.len(), 31);
    assert_eq!(r.dropped_packets_buffer_full, 70);
    assert_eq!(r.dropped_packets_loss, 0);
    assert_eq!(r.dropped_packets_limiter, 0);
}

#[test]
fn capacity_drops_are_counted_as_buffer_full_in_fifo_mode() {
    capacity_causes_buffer_full_drops(false);
}

#[test]
fn capacity_drops_are_counted_as_buffer_full_in_reordering_mode() {
    capacity_causes_buffer_full_drops(true);
}

#[test]
fn zero_capacity_rejects_every_packet_as_buffer_full() {
    let cfg = scenario().with("buffer_capacity", json!(0)).build();
    let (r, deps) = trace(&cfg, 0);
    assert!(deps.is_empty());
    assert_eq!(r.tracked_packets, 0);
    assert_eq!(r.dropped_packets_buffer_full, 101);
    assert_eq!(r.dropped_packets_loss + r.dropped_packets_limiter, 0);
}

// ----------------------------------------------------------------------- loss

#[test]
fn random_loss_one_loses_every_packet() {
    let cfg = scenario()
        .with("loss_model", json!({"Random": 1.0}))
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert!(deps.is_empty());
    assert_eq!(r.tracked_packets, 0);
    assert_eq!(r.dropped_packets_loss, 101);
    assert_eq!(r.dropped_packets_limiter + r.dropped_packets_buffer_full, 0);
    assert!(r.delay_mean_ns.is_finite() && r.inter_arrival_time_mean_ns.is_finite());
}

#[test]
fn random_loss_zero_and_default_lose_nothing() {
    for cfg in [
        scenario().build(),
        scenario()
            .with("loss_model", json!({"Random": 0.0}))
            .build(),
    ] {
        let (r, deps) = trace(&cfg, 0);
        assert_eq!(deps.len(), 101);
        assert_eq!(total_drops(&r), 0);
    }
}

#[test]
fn random_loss_probability_is_respected_statistically() {
    // 20001 arrivals, p = 0.3: expected 6000.3 lost, sigma = 65; +-400 = 6 sigma.
    let cfg = scenario()
        .with("duration_s", json!(0.2))
        .with("loss_model", json!({"Random": 0.3}))
        .build();
    let (r, deps) = trace(&cfg, 0);
    let arrivals = expected_arrivals(&cfg);
    assert_eq!(arrivals, 20_001);
    assert_eq!(r.dropped_packets_loss + deps.len() as u64, arrivals);
    assert!(
        r.dropped_packets_loss.abs_diff(6_000) <= 400,
        "lost {}",
        r.dropped_packets_loss
    );
    assert_eq!(r.dropped_packets_limiter + r.dropped_packets_buffer_full, 0);
    for pair in deps.windows(2) {
        assert!(pair[0].seq < pair[1].seq, "FIFO order with gaps");
    }
}

#[test]
fn gilbert_elliot_loss_model_is_applied() {
    // good -> bad with probability 1, never back; k = 1 (good never loses),
    // h = 0 (bad always loses): first packet survives, all others are lost.
    let cfg = scenario()
        .with(
            "loss_model",
            json!({"GilbertElliot": [1.0, 0.0, 1.0, 0.0, true]}),
        )
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].seq, 0);
    assert_eq!(r.dropped_packets_loss, 100);
}

/// TBF at 0.5 bit/ns with capacity 1 (bucket 12336 bit, starts full), zero
/// delay: the bucket gains 5000 bit per arrival and a packet costs 12032 bit,
/// so every third arrival (k % 3 == 0: 34 of 101) is admitted and the other 67
/// are rejected by the limiter.
fn tight_token_bucket() -> Scenario {
    scenario().capacity(1).limiter("TokenBucketFilter", 0.5e9)
}

#[test]
fn loss_takes_precedence_over_limiter_drop_in_the_counters() {
    // The limiter would reject 67 of the packets, the loss model rejects
    // every packet: dpds-core reports Lost.
    let cfg = tight_token_bucket()
        .with("loss_model", json!({"Random": 1.0}))
        .build();
    let r = run_simulation(&cfg, 0);
    assert_eq!(r.dropped_packets_loss, 101);
    assert_eq!(r.dropped_packets_limiter, 0);
    // without loss the same limiter drops those 67
    let (r, deps) = trace(&tight_token_bucket().build(), 0);
    assert_eq!(r.dropped_packets_limiter, 67);
    assert_eq!(r.dropped_packets_loss, 0);
    assert_eq!(r.dropped_packets_buffer_full, 0);
    let seqs: Vec<u64> = deps.iter().map(|d| d.seq).collect();
    assert_eq!(seqs, (0..=99).step_by(3).collect::<Vec<u64>>());
}

#[test]
fn buffer_full_takes_precedence_over_loss_and_limiter_in_the_counters() {
    // Capacity 0: the buffer rejects everything before the loss model (which
    // would also reject everything) or the limiter (derived limit 0 bit,
    // would reject everything) is consulted.
    for limiter in ["Spacer", "TokenBucketFilter"] {
        let cfg = scenario()
            .capacity(0)
            .with("loss_model", json!({"Random": 1.0}))
            .limiter(limiter, 0.5e9)
            .build();
        let (r, deps) = trace(&cfg, 0);
        assert!(deps.is_empty(), "{limiter}");
        assert_eq!(r.dropped_packets_buffer_full, 101, "{limiter}");
        assert_eq!(r.dropped_packets_loss, 0, "{limiter}");
        assert_eq!(r.dropped_packets_limiter, 0, "{limiter}");
    }
}

/// Capacity 1, delay 1 ms, Gilbert-Elliot with p = 1, k = 1, h = 0 (first
/// packet survives, every later one is lost). Arrival 0 is accepted and
/// fills the buffer; 1..99 are rejected as buffer-full and must not consume a
/// loss draw (the model would move on to its lossy state); at arrival 100
/// (packet 0 leaves first, tie rule) the model is therefore still at its
/// second draw and loses it. 99 buffer-full, 1 lost, 1 accepted. (With the old
/// order the model had already been advanced by arrival 1: 100 lost, 0 full.)
#[test]
fn buffer_full_packets_consume_no_loss_draw() {
    let cfg = scenario()
        .capacity(1)
        .delay("DeterministicDistribution", 1_000_000., 0.)
        .with(
            "loss_model",
            json!({"GilbertElliot": [1.0, 0.0, 1.0, 0.0, true]}),
        )
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].seq, 0);
    assert_eq!(r.dropped_packets_buffer_full, 99);
    assert_eq!(r.dropped_packets_loss, 1);
    assert_eq!(r.dropped_packets_limiter, 0);
}

#[test]
fn drop_causes_are_conserved_with_all_three_active() {
    let cfg = scenario()
        .with("duration_s", json!(0.01))
        .with("loss_model", json!({"Random": 0.2}))
        .with("buffer_capacity", json!(10))
        .delay("NormalDistribution", 100_000., 30_000.)
        .limiter("Spacer", 0.9e9)
        .build();
    let (r, deps) = trace(&cfg, 0);
    assert_eq!(deps.len() as u64 + total_drops(&r), expected_arrivals(&cfg));
    assert!(r.dropped_packets_loss > 0);
    assert!(r.dropped_packets_limiter + r.dropped_packets_buffer_full > 0);
}

// ----------------------------------------------------------------------- seed

fn noisy() -> Scenario {
    scenario()
        .with("duration_s", json!(0.002))
        .with("loss_model", json!({"Random": 0.1}))
        .delay("NormalDistribution", 1_000_000., 300_000.)
}

#[test]
fn same_seed_gives_identical_results_and_trace() {
    for reordering in [false, true] {
        let cfg = noisy().reorder(reordering).build();
        let (r1, d1) = trace(&cfg, 0);
        let (r2, d2) = trace(&cfg, 0);
        assert_eq!(d1, d2);
        assert_eq!(format!("{r1:?}"), format!("{r2:?}"));
        let (_, row1) = csv_row_of("seed-same-1", &r1, 0);
        let (_, row2) = csv_row_of("seed-same-2", &r2, 0);
        assert_eq!(row1, row2);
    }
}

#[test]
fn different_seeds_give_different_results() {
    let (_, d1) = trace(&noisy().with("seed", json!(1)).build(), 0);
    let (_, d2) = trace(&noisy().with("seed", json!(2)).build(), 0);
    assert_ne!(d1, d2);
}

#[test]
fn run_r_with_seed_s_equals_run_zero_with_seed_s_plus_r() {
    let base = noisy().with("seed", json!(1000));
    let (_, run0) = trace(&base.build(), 0);
    let (_, run3) = trace(&base.build(), 3);
    let (_, shifted) = trace(&base.clone().with("seed", json!(1003)).build(), 0);
    assert_eq!(run3, shifted);
    assert_ne!(run0, run3);
}

#[test]
fn unseeded_runs_complete() {
    let mut cfg = noisy().build();
    cfg.simulation_config.seed = None;
    let r = run_simulation(&cfg, 0);
    assert_eq!(
        r.tracked_packets as u64 + total_drops(&r),
        expected_arrivals(&cfg)
    );
}
