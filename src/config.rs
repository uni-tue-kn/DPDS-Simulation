use crate::statistics::ReorderingMetric;
use dpds_core::{delayer::RVDistribution, limiter::LimiterMode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub simulation_config: SimulationConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_config: Option<DelayConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limiter_config: Option<LimiterConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationConfig {
    pub duration_s: f64,
    pub runs: u32,
    pub rate_gbps: f64,
    pub average_frame_size_byte: f64,
    pub activate_reordering: bool,
    pub csv_output_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reordering_metric: Option<ReorderingMetric>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelayConfig {
    pub mean_ns: f64,
    pub std_ns: f64,
    pub rv_distribution: RVDistribution,
    pub activate_correlation: bool,
    pub half_life_period_ns: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimiterConfig {
    pub mode: LimiterMode,
    pub rate_bps: f64,
    pub limit_bit: f64,
}
