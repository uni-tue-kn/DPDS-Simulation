//! Characterisation tests of config parsing that stays in the redesign
//! (the new optional fields are tested in `model.rs`).

use dpds_core::{delayer::RVDistribution, limiter::LimiterMode};
use lib::config::Config;
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
            "mode": "TokenBucketFilter", "rate_bps": 200000000, "limit_bit": 6152000000
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
    assert_eq!(l.limit_bit, 6_152_000_000.);
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
        let json = format!(r#"{{"mode":"{name}","rate_bps":1,"limit_bit":1}}"#);
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
            assert!(l.rate_bps > 0. && l.limit_bit > 0.);
        }
    }
}
