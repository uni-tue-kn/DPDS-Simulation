//! Characterisation tests of config parsing that stays in the redesign
//! (the new optional fields are tested in `model.rs`).

use dpds_core::{LimiterConfig as CoreLimiterConfig, buffer::Buffer};
use dpds_core::{delayer::RVDistribution, limiter::LimiterMode};
use lib::config::Config;
use lib::simulation::buffer_config;
use lib::statistics::ReorderingMetric;
use std::collections::HashSet;

fn parse(json: &str) -> Config {
    serde_json::from_str(json).expect("config must parse")
}

const MINIMAL: &str = r#"{
    "simulation_config": {
        "duration_s": 0.5, "runs": 3, "rate_gbps": 1.0,
        "average_frame_size_byte": 1518, "activate_reordering": false,
        "csv_output_path": "x.csv"
    }
}"#;

#[test]
fn minimal_config_has_no_optional_sections() {
    let c = parse(MINIMAL);
    assert_eq!(c.simulation_config.duration_s, 0.5);
    assert_eq!(c.simulation_config.runs, 3);
    assert_eq!(c.simulation_config.rate_gbps, 1.0);
    assert_eq!(c.simulation_config.average_frame_size_byte, 1518.);
    assert!(!c.simulation_config.activate_reordering);
    assert_eq!(c.simulation_config.csv_output_path, "x.csv");
    assert!(c.simulation_config.reordering_metric.is_none());
    assert!(c.delay_config.is_none());
    assert!(c.limiter_config.is_none());
}

#[test]
fn full_config_parses_all_sections() {
    let c = parse(
        r#"{
        "simulation_config": {
            "duration_s": 600, "runs": 10, "rate_gbps": 10,
            "average_frame_size_byte": 1518, "activate_reordering": true,
            "csv_output_path": "y.csv", "reordering_metric": "kendall_tau_distance"
        },
        "delay_config": {
            "mean_ns": 10000000, "std_ns": 300000,
            "rv_distribution": "GammaDistribution",
            "activate_correlation": true, "half_life_period_ns": 21000000
        },
        "limiter_config": {
            "mode": "TokenBucketFilter", "rate_bps": 200000000
        }
    }"#,
    );
    assert!(c.simulation_config.activate_reordering);
    assert_eq!(
        c.simulation_config.reordering_metric,
        Some(ReorderingMetric::KendallTauDistance)
    );
    let d = c.delay_config.unwrap();
    assert_eq!(d.mean_ns, 10_000_000.);
    assert_eq!(d.std_ns, 300_000.);
    assert!(matches!(
        d.rv_distribution,
        RVDistribution::GammaDistribution
    ));
    assert!(d.activate_correlation);
    assert_eq!(d.half_life_period_ns, 21_000_000.);
    let l = c.limiter_config.unwrap();
    assert!(matches!(l.mode, LimiterMode::TokenBucketFilter));
    assert_eq!(l.rate_bps, 200_000_000.);
}

#[test]
fn every_delay_distribution_and_limiter_mode_name_parses() {
    for name in [
        "DeterministicDistribution",
        "NormalDistribution",
        "LogNormalDistribution",
        "GammaDistribution",
        "UniformDistribution",
    ] {
        let json = format!(
            r#"{{"mean_ns":1,"std_ns":1,"rv_distribution":"{name}","activate_correlation":false,"half_life_period_ns":0}}"#
        );
        serde_json::from_str::<lib::config::DelayConfig>(&json).expect(name);
    }
    for name in ["Spacer", "TokenBucketFilter"] {
        let json = format!(r#"{{"mode":"{name}","rate_bps":1}}"#);
        serde_json::from_str::<lib::config::LimiterConfig>(&json).expect(name);
    }
}

#[test]
fn missing_required_field_is_rejected() {
    let without_runs = MINIMAL.replace("\"runs\": 3,", "");
    assert!(serde_json::from_str::<Config>(&without_runs).is_err());
    let bad_metric = MINIMAL.replace(
        "\"csv_output_path\": \"x.csv\"",
        "\"csv_output_path\": \"x.csv\", \"reordering_metric\": \"nope\"",
    );
    assert!(serde_json::from_str::<Config>(&bad_metric).is_err());
}

#[test]
fn config_serialization_round_trips_and_skips_none_options() {
    let c = parse(MINIMAL);
    let value = serde_json::to_value(&c).unwrap();
    assert!(value.get("delay_config").is_none());
    assert!(value.get("limiter_config").is_none());
    assert!(
        value["simulation_config"]
            .get("reordering_metric")
            .is_none()
    );
    let back: Config = serde_json::from_value(value).unwrap();
    assert_eq!(back.simulation_config.runs, 3);
    assert_eq!(back.simulation_config.csv_output_path, "x.csv");
}

#[test]
fn simulation_configs_json_parses_and_is_well_formed() {
    let text = include_str!("../simulation_configs.json");
    let configs: Vec<Config> = serde_json::from_str(text).expect("simulation_configs.json");
    assert!(!configs.is_empty());
    let mut paths = HashSet::new();
    for c in &configs {
        let s = &c.simulation_config;
        assert!(s.duration_s > 0. && s.runs > 0, "{}", s.csv_output_path);
        assert!(s.rate_gbps > 0. && s.average_frame_size_byte > 0.);
        assert!(s.csv_output_path.ends_with(".csv"));
        assert!(
            paths.insert(s.csv_output_path.clone()),
            "duplicate csv_output_path {}",
            s.csv_output_path
        );
        if let Some(d) = &c.delay_config {
            assert!(d.mean_ns > 0. && d.std_ns >= 0. && d.half_life_period_ns >= 0.);
        }
        if let Some(l) = &c.limiter_config {
            assert!(l.rate_bps > 0.);
        }
    }
}

#[test]
fn simulation_configs_json_has_no_limit_bit_anywhere() {
    // The limiter's limit is derived from `buffer_capacity` by dpds-core.
    let text = include_str!("../simulation_configs.json");
    assert!(!text.contains("limit_bit"), "stale limit_bit key");
    let configs: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
    for c in configs {
        if let Some(l) = c.get("limiter_config") {
            let mut keys: Vec<&String> = l.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, ["mode", "rate_bps"], "{c}");
        }
    }
}

#[test]
fn every_shipped_config_builds_a_buffer() {
    // The limiter limit is derived from the capacity, so `buffer_config` must
    // yield a config dpds-core accepts for every committed config (including
    // the default 2^21 capacity), for several runs.
    let text = include_str!("../simulation_configs.json");
    let configs: Vec<Config> = serde_json::from_str(text).unwrap();
    assert!(configs.iter().any(|c| c.limiter_config.is_some()));
    for c in &configs {
        for run in [0, 1] {
            Buffer::try_new(buffer_config(c, run)).unwrap_or_else(|e| {
                panic!("{}: run {run}: {e}", c.simulation_config.csv_output_path)
            });
        }
    }
}

// ------------------------------------------------- the limiter has no limit

const LIMITER_CONFIG: &str = r#"{"mode": "Spacer", "rate_bps": 200000000}"#;

#[test]
fn limiter_config_with_limit_bit_is_rejected_by_name() {
    // dpds-core derives the limit from the buffer capacity; a leftover
    // `limit_bit` is an error (as in dpds-core's own parser), not ignored.
    for mode in ["Spacer", "TokenBucketFilter"] {
        let json =
            format!(r#"{{"mode": "{mode}", "rate_bps": 200000000, "limit_bit": 6152000000}}"#);
        let err = serde_json::from_str::<lib::config::LimiterConfig>(&json)
            .expect_err("limit_bit must be rejected")
            .to_string();
        assert!(err.contains("limit_bit"), "{err}");
    }
}

#[test]
fn config_with_limit_bit_in_the_limiter_is_rejected_by_name() {
    let json = MINIMAL.replace(
        "\n}",
        r#", "limiter_config": {"mode": "Spacer", "rate_bps": 1, "limit_bit": 1}
}"#,
    );
    let err = serde_json::from_str::<Config>(&json)
        .expect_err("limit_bit must be rejected")
        .to_string();
    assert!(err.contains("limit_bit"), "{err}");
    // the same config without it parses
    let ok = json.replace(r#", "limit_bit": 1"#, "");
    assert!(serde_json::from_str::<Config>(&ok).is_ok());
}

#[test]
fn limiter_config_rejects_other_unknown_keys_and_missing_keys() {
    // strict, not just a special case for `limit_bit`
    for json in [
        r#"{"mode": "Spacer", "rate_bps": 1, "burst_bit": 1}"#,
        r#"{"mode": "Spacer", "rate_bps": 1, "activate": true}"#,
        r#"{"mode": "Spacer"}"#,
        r#"{"rate_bps": 1}"#,
    ] {
        assert!(
            serde_json::from_str::<lib::config::LimiterConfig>(json).is_err(),
            "{json}"
        );
    }
}

#[test]
fn limiter_config_serializes_to_mode_and_rate_only() {
    let l: lib::config::LimiterConfig = serde_json::from_str(LIMITER_CONFIG).unwrap();
    let value = serde_json::to_value(&l).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"mode": "Spacer", "rate_bps": 200000000.0})
    );
    let back: lib::config::LimiterConfig = serde_json::from_value(value).unwrap();
    assert_eq!(back.rate_bps, 200_000_000.);
}

fn config_with(limiter: Option<&str>, capacity: Option<usize>) -> Config {
    let mut v: serde_json::Value = serde_json::from_str(MINIMAL).unwrap();
    if let Some(l) = limiter {
        v["limiter_config"] = serde_json::from_str(l).unwrap();
    }
    if let Some(c) = capacity {
        v["simulation_config"]["buffer_capacity"] = c.into();
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn buffer_config_passes_mode_and_rate_to_a_limiter_without_a_limit() {
    for (name, mode_is_spacer) in [("Spacer", true), ("TokenBucketFilter", false)] {
        let json = format!(r#"{{"mode": "{name}", "rate_bps": 200000000}}"#);
        let bc = buffer_config(&config_with(Some(&json), Some(7)), 0);
        // exhaustive destructuring: dpds-core's limiter config has exactly
        // these fields (no `limit_bit`)
        let CoreLimiterConfig {
            mode,
            activate,
            rate_bps,
        } = bc.limiter;
        assert_eq!(
            matches!(mode, LimiterMode::Spacer),
            mode_is_spacer,
            "{name}"
        );
        assert!(activate, "{name}");
        assert_eq!(rate_bps, 200_000_000., "{name}");
        // the capacity reaches the buffer, which derives the limit from it
        assert_eq!(bc.buffer_capacity, 7);
        let buffer = Buffer::try_new(bc).expect("builds");
        assert_eq!(buffer.capacity(), 7);
    }
}

#[test]
fn buffer_config_without_a_limiter_config_deactivates_the_limiter() {
    for capacity in [None, Some(5)] {
        let bc = buffer_config(&config_with(None, capacity), 0);
        let CoreLimiterConfig { activate, .. } = bc.limiter;
        assert!(!activate);
        Buffer::try_new(bc).expect("builds");
    }
}
