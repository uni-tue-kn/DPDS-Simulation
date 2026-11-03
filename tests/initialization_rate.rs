//! The correlated-delay start-up rate (`DelayerConfig::initialization_rate_pps`
//! of `dpds-core`, required, no default) is always the simulation's own offered
//! packet rate, never a constant and never a config-file field.
//!
//! # Test-facing API fixed by these tests
//!
//! ```text
//! lib::simulation::buffer_config(config: &Config, run: u32) -> dpds_core::Config
//!     // already exists privately; must become `pub`. Its
//!     // `.delayer.initialization_rate_pps` must be 1e9 / iat_ns with
//!     // iat_ns = 1e9 / (rate_gbps * 1e9 / ((average_frame_size_byte + 20) * 8)),
//!     // i.e. the arrival rate of the fixed inter-arrival process (the frame size
//!     // is NOT rounded here, unlike the length handed to the buffer), for every
//!     // config: with or without `delay_config`, with or without correlation.
//! ```
//!
//! # Correlation math behind the behavioural test (dpds-core `delayer.rs`)
//!
//! With rate `r` (packets/ns) and half-life `h` (ns): `k = h * r`, weight
//! `w = 2^(-1/k)`, innovation std `s_r = sigma * sqrt((1 + w) / (1 - w))`, and
//! `x_i = w * x_(i-1) + (1 - w) * N(mean, s_r)` starting at `x_(-1) = mean`. The
//! rate estimate is refreshed only once more than 1 ms has passed since the
//! first packet.
//!
//! Scenario: 0.1 Gbit/s, 1518 B frames -> iat = 123 040 ns (8127 pps), so the
//! first refresh happens at packet 9 (`9 * iat > 1 ms`, `8 * iat < 1 ms`) and
//! packets 0..=7 use the initial weight.
//!
//! * Offered rate: `k` = 56.9, `w` = 0.9879, one step of `(1 - w) * s_r` is
//!   about 15.5 us (std), so the eight delays wander by tens of us.
//! * Old hard-coded 15 Mpps: `k` = 105 000, `w` = 1 - 6.6e-6, `s_r` = 550 sigma,
//!   one step is about 0.36 us, so the eight delays stay within about 1 us.

use dpds_core::buffer::Buffer;
use lib::config::Config;
use lib::simulation::{buffer_config, run_simulation_traced};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

fn config_from(
    rate_gbps: f64,
    frame_byte: f64,
    delay: Option<Value>,
    limiter: Option<Value>,
) -> Config {
    let mut v = json!({
        "simulation_config": {
            "duration_s": 0.002, "runs": 1, "rate_gbps": rate_gbps,
            "average_frame_size_byte": frame_byte, "activate_reordering": false,
            "csv_output_path": "x.csv", "seed": 7
        }
    });
    if let Some(d) = delay {
        v["delay_config"] = d;
    }
    if let Some(l) = limiter {
        v["limiter_config"] = l;
    }
    serde_json::from_value(v).expect("config must parse")
}

fn delay_json(correlation: bool, half_life_ns: f64) -> Value {
    json!({
        "mean_ns": 10_000_000, "std_ns": 100_000,
        "rv_distribution": "NormalDistribution",
        "activate_correlation": correlation, "half_life_period_ns": half_life_ns
    })
}

/// The offered packet rate by the spec's formula: 1e9 / iat_ns.
fn offered_pps(rate_gbps: f64, frame_byte: f64) -> f64 {
    let rate_pps = rate_gbps * 1e9 / ((frame_byte + 20.) * 8.);
    let iat_ns = 1e9 / rate_pps;
    1e9 / iat_ns
}

fn assert_rate(config: &Config, run: u32, what: &str) {
    let sim = &config.simulation_config;
    let expected = offered_pps(sim.rate_gbps, sim.average_frame_size_byte);
    let got = buffer_config(config, run).delayer.initialization_rate_pps;
    assert!(
        (got - expected).abs() <= expected * 1e-12,
        "{what}: initialization_rate_pps = {got}, expected the offered rate {expected}"
    );
}

#[test]
fn initialization_rate_is_the_offered_packet_rate() {
    // (rate Gbit/s, frame B): the four rates of simulation_configs.json, other
    // frame sizes, and a fractional frame size (the formula does not round it).
    let cases = [
        (0.1, 1518.),
        (1., 1518.),
        (10., 1518.),
        (100., 1518.),
        (1., 64.),
        (40., 800.),
        (25., 1480.),
        (3.3, 800.5),
    ];
    for (rate_gbps, frame_byte) in cases {
        let config = config_from(
            rate_gbps,
            frame_byte,
            Some(delay_json(true, 7_000_000.)),
            None,
        );
        assert_rate(&config, 0, &format!("{rate_gbps} Gbit/s, {frame_byte} B"));
    }
}

#[test]
fn initialization_rate_matches_exact_hand_values() {
    // 1.2 Gbit/s, 1480 B -> 12000 bit on the wire -> 100 000 packets/s.
    let config = config_from(1.2, 1480., Some(delay_json(true, 7_000_000.)), None);
    let got = buffer_config(&config, 0).delayer.initialization_rate_pps;
    assert!((got - 100_000.).abs() < 1e-6, "got {got}");
    // 0.1 Gbit/s, 1518 B -> 12304 bit -> 1e8 / 12304 = 8127.4... packets/s.
    let config = config_from(0.1, 1518., Some(delay_json(true, 7_000_000.)), None);
    let got = buffer_config(&config, 0).delayer.initialization_rate_pps;
    assert!((got - 1e8 / 12_304.).abs() < 1e-6, "got {got}");
}

#[test]
fn initialization_rate_is_set_in_every_config_shape() {
    let limiter = json!({"mode": "Spacer", "rate_bps": 1e8, "limit_bit": 1e9});
    let variants = [
        ("no delay, no limiter", None, None),
        ("no delay, limiter", None, Some(limiter.clone())),
        (
            "delay without correlation",
            Some(delay_json(false, 0.)),
            None,
        ),
        (
            "delay with correlation, limiter",
            Some(delay_json(true, 21_000_000.)),
            Some(limiter),
        ),
    ];
    for (what, delay, limiter) in variants {
        let config = config_from(10., 1518., delay, limiter);
        // Also for run > 0: the rate does not depend on the run.
        for run in [0, 3] {
            assert_rate(&config, run, what);
        }
        // The buffer must build: dpds-core validates the rate even without
        // correlation, so a missing or invalid value would fail here.
        Buffer::try_new(buffer_config(&config, 0))
            .unwrap_or_else(|e| panic!("{what}: Buffer::try_new failed: {e}"));
    }
}

#[test]
fn initialization_rate_follows_the_offered_load_not_a_constant() {
    let rate = |gbps: f64| {
        let config = config_from(gbps, 1518., Some(delay_json(true, 7_000_000.)), None);
        buffer_config(&config, 0).delayer.initialization_rate_pps
    };
    // Ten times the bit rate, ten times the packet rate.
    assert!((rate(10.) / rate(1.) - 10.).abs() < 1e-9);
    assert!((rate(1.) / rate(0.1) - 10.).abs() < 1e-9);
    assert_ne!(rate(0.1), 15_000_000.);
}

fn read_configs() -> (String, Vec<Config>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("simulation_configs.json");
    let text = fs::read_to_string(&path).expect("read simulation_configs.json");
    let configs = serde_json::from_str(&text).expect("simulation_configs.json must parse");
    (text, configs)
}

#[test]
fn every_shipped_config_builds_a_buffer_with_the_offered_rate() {
    let (_, configs) = read_configs();
    assert!(!configs.is_empty());
    for (i, config) in configs.iter().enumerate() {
        let what = format!(
            "simulation_configs.json entry {i} ({})",
            config.simulation_config.csv_output_path
        );
        let buffer_config = buffer_config(config, 0);
        assert!(buffer_config.delayer.initialization_rate_pps >= 1.);
        assert_rate(config, 0, &what);
        Buffer::try_new(buffer_config)
            .unwrap_or_else(|e| panic!("{what}: Buffer::try_new failed: {e}"));
    }
}

#[test]
fn shipped_configs_and_config_types_have_no_initialization_rate_field() {
    let (text, _) = read_configs();
    assert!(
        !text.contains("initialization_rate"),
        "the rate is derived, not a field of simulation_configs.json"
    );
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config.rs"))
        .expect("read src/config.rs");
    assert!(
        !src.contains("initialization_rate"),
        "src/config.rs must not take an initialization_rate field"
    );
}

#[test]
fn no_preset_rate_constant_in_src() {
    let forbidden = ["15_000_000", "15000000", "15e6", "15.0e6", "15e+6", "mpps"];
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = 0;
    for entry in fs::read_dir(&src_dir).expect("read src/") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        files += 1;
        let text = fs::read_to_string(&path).expect("read source file");
        for (n, line) in text.lines().enumerate() {
            // Comments may talk about the old default; code may not use it.
            let code = line.split("//").next().unwrap_or("").to_lowercase();
            for token in forbidden {
                assert!(
                    !code.contains(&token.to_lowercase()),
                    "{}:{}: preset rate constant `{token}`: {line}",
                    path.display(),
                    n + 1
                );
            }
        }
    }
    assert!(files >= 5, "found only {files} source files in src/");
}

/// Standard deviation (n - 1) of the first `n` delays (`send_time - rx_time`)
/// of one correlated-delay run; the run index selects the seed (`7 + run`).
fn early_delay_std_ns(config: &Config, run: u32, n: usize) -> f64 {
    let mut delays = Vec::new();
    run_simulation_traced(config, run, |d| {
        if delays.len() < n {
            delays.push(d.send_time_ns as f64 - d.rx_time_ns);
        }
    });
    assert_eq!(delays.len(), n, "the run must depart at least {n} packets");
    let mean = delays.iter().sum::<f64>() / n as f64;
    (delays.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
}

#[test]
fn correlated_delay_has_early_spread_at_a_low_rate() {
    // See the module docs: packets 0..=7 use the initial weight; measured
    // (relative, debug build): mean per-run std of those eight delays is 18 us
    // with the offered rate and 0.43 us with the old 15 Mpps. Threshold 5 us:
    // several times below the measured value (and the mean over 32 seeded runs
    // is used, so seed luck cannot trip it) and 10 times above the old
    // behaviour.
    let config = config_from(0.1, 1518., Some(delay_json(true, 7_000_000.)), None);
    let runs = 32;
    let mean_std_ns = (0..runs)
        .map(|run| early_delay_std_ns(&config, run, 8))
        .sum::<f64>()
        / f64::from(runs);
    assert!(
        mean_std_ns > 5_000.,
        "early delay spread {mean_std_ns} ns: the start-up rate looks too high \
         (a 15 Mpps start-up gives about 500 ns, the offered rate tens of us)"
    );
    // And not absurd: the stationary std is 100 us, early values stay below it.
    assert!(
        mean_std_ns < 100_000.,
        "early delay spread {mean_std_ns} ns"
    );
}
